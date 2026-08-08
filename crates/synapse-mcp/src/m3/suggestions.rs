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
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Local, TimeZone, Timelike};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::error_codes;
use synapse_core::intent::IntentCandidate;
use synapse_core::types::{
    EpisodeRecord, RoutineFeedbackOutcome, RoutineGranularity, RoutineLifecycle, SubsystemHealth,
};
use synapse_core::{SCHEMA_VERSION, StoredEvent};
use synapse_storage::{
    Db, SYN_EPISODE_PANEL_NAME, SYN_EPISODE_PANEL_VERSION, cf, decode_json, encode_json,
};

use crate::m1::mcp_error;

use super::episodes::{decode_episode_row, hex_decode, key_after, now_ts_ns, recent_episode_rows};
use super::grounding::{self, SOURCE_OPERATOR};
use super::intent::{IntentCurrentParams, current_intents};
use super::permissions::{Permission, RequiredPermissions, required};
use super::plan::{PlanBackend, PlanDocument, PlanStep, Postcondition};
use super::plan_execution::PlanExecutionRecord;
use super::routines::{
    RoutineFeedbackParams, feedback_cooldown_secs, feedback_suppressed, load_state_row,
    record_routine_feedback,
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

// --- Kernel/graph next-action composition (#2046, #1690 clause 2) ---

/// `CF_KV` key of the pointer row naming the current frozen next-action artifact.
const NEXT_ACTION_CURRENT_KEY: &str = "assist_next_action/v1/current";
/// `CF_KV` key prefix of the content-addressed frozen next-action artifacts.
const NEXT_ACTION_FROZEN_PREFIX: &str = "assist_next_action/v1/frozen/";
/// Schema version for [`NextActionArtifact`] and its pointer row.
const NEXT_ACTION_ARTIFACT_RECORD_VERSION: u32 = 1;
/// Routine-id prefix for a composed next-action suggestion.
const NEXT_ACTION_ROUTINE_PREFIX: &str = "next1-";
/// How long a frozen artifact is trusted before the composer re-runs.
const DEFAULT_NEXT_ACTION_STALENESS_SECS: u64 = 3_600;
/// Context lookback handed to the atomic context measurement (6h).
const DEFAULT_NEXT_ACTION_CONTEXT_LOOKBACK_SECS: u64 = 21_600;
const MAX_NEXT_ACTION_CONTEXT_LOOKBACK_SECS: u64 = 604_800;
/// Kernel-graph hop budget for one composition.
const DEFAULT_NEXT_ACTION_MAX_HOPS: u32 = 4;
const MAX_NEXT_ACTION_MAX_HOPS: u32 = 16;
/// Context episodes read per composition (newest first).
const NEXT_ACTION_CONTEXT_ROWS: usize = 32;
/// Hard cap on composed candidates in one artifact.
const MAX_NEXT_ACTION_CANDIDATES: usize = 8;
/// Anchor kind stamped on an accept/decline of any suggestion.
const SUGGESTION_OUTCOME_ANCHOR_KIND: &str = "synapse:suggestion_outcome";
/// Anchor kind stamped on the episode rows that GENERATED a next-action
/// suggestion, so the decision lands on the evidence, not only on the offer.
const NEXT_ACTION_EVIDENCE_ANCHOR_KIND: &str = "synapse:suggestion_next_action_outcome";
const SUGGESTION_ANCHOR_SOURCE_OF_TRUTH: &str =
    "Calyx Anchors CF (independently rescanned) + Calyx provenance ledger";
const NEXT_ACTION_SOURCE_OF_TRUTH: &str = "CF_KV assist_next_action/v1 frozen artifact derived from Calyx Kernel/Base CF over CF_EPISODES";

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
    /// Composed from the Calyx domain kernel plus the between-record graph over
    /// `CF_EPISODES` (#2046). Always carries [`SuggestionRecord::next_action`];
    /// a next-action suggestion without its grounding evidence cannot exist.
    KernelNextAction,
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

/// One hop of the kernel-graph evidence path that produced a next action.
///
/// `from`/`to` are Calyx constellation ids; `edge_weight` is the measured
/// between-record association strength and `hop_score` is Calyx's attenuated
/// path score (`edge_weight · 0.9^hop`). Nothing here is estimated locally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NextActionHop {
    pub from_cx_id: String,
    pub to_cx_id: String,
    pub edge_weight: f64,
    pub hop_index: u32,
    pub hop_score: f64,
}

/// The complete grounding evidence for one composed next action.
///
/// Every field names a physical row or a Calyx-measured quantity: which
/// `CF_EPISODES` row supplied the context, which constellation it measured to,
/// which kernel answered, which graph edges were walked, which Base row the
/// terminal kernel node points at, and which `CF_EPISODES` row that Base row
/// resolves back to. A suggestion is never built without this — an ungrounded
/// next action is not a weak suggestion, it is a refused one (#2046).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NextActionGrounding {
    pub source_of_truth: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub content_slot: u32,
    pub kernel_id: String,
    pub kernel_members: u64,
    pub anchor_kernel_node: String,
    /// Kernel-only recall against the full corpus, and the gate it had to clear.
    pub recall_ratio: f64,
    pub min_recall_ratio: f64,
    /// Calyx's own total score for the answered path.
    pub total_score: f64,
    pub context_episode_id: String,
    pub context_episode_key_hex: String,
    pub context_cx_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_app: Option<String>,
    pub context_start_ts_ns: u64,
    pub hops: Vec<NextActionHop>,
    pub target_cx_id: String,
    pub target_source_cf: String,
    pub target_source_key_hex: String,
    pub target_episode_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_document: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_url: Option<String>,
    /// `hop_score · recall_ratio`: both factors measured, neither invented.
    pub grounded_confidence: f64,
    /// Content fingerprint of the frozen artifact this candidate was lowered
    /// into. A hot-path consumer reads that artifact, never Calyx.
    pub artifact_sha256: String,
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
    /// Kernel/graph grounding evidence. Required for
    /// [`SuggestionSource::KernelNextAction`], absent for every other source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<NextActionGrounding>,
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
        RoutineLifecycle::Disabled | RoutineLifecycle::Archived | RoutineLifecycle::Quarantined
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
    /// Include kernel/graph-composed next actions in the same gated pass
    /// (#2046). The common path reads the frozen artifact and issues no live
    /// Calyx call.
    #[serde(default = "default_true")]
    pub include_next_actions: bool,
    /// Force recomposition from Calyx even when the frozen artifact is fresh.
    #[serde(default)]
    pub refresh_next_actions: bool,
    /// Context lookback for the atomic next-action context measurement.
    /// Defaults to 6h, capped at 7 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action_context_lookback_secs: Option<u64>,
    /// Kernel-graph hop budget per composition (default 4, max 16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action_max_hops: Option<u32>,
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
    /// What the kernel/graph next-action composer did this pass — including the
    /// verbatim Calyx refusal when it stayed silent (#2046).
    pub next_action: NextActionCompositionReport,
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

// === Kernel/graph next-action composition (#2046) ===
//
// The #1690 clause: "given current context (foreground app, time, recent
// episodes), kernel + between-record graph propose the operator's historically-
// next steps with grounded confidence; honesty-gated (silent when evidence is
// insufficient — no noise)."
//
// The pipeline follows the handbook loop in `docs/BUILDING_ON_CALYX.md` §6.2
// exactly, and does not shortcut any stage:
//
//   ① DECOMPOSE  — the atomic context measurement is the newest `CF_EPISODES`
//                  row, measured into its native episode constellation.
//   ② ASSOCIATE  — Calyx's own between-record nearest-neighbour graph over the
//                  episode panel supplies the edges; we never build our own.
//   ③ DIFFERENTIATE — the kernel's recall gate decides whether the graph
//                  explains the corpus at all; below the gate Calyx refuses.
//   ④ DISTILL→COMPOSE — `kernel_answer` walks the grounded path; each terminal
//                  kernel node is resolved back through its Base source pointer
//                  to the physical `CF_EPISODES` row that IS the next action.
//   ⑧ LOWER      — the result is frozen into a content-addressed `CF_KV`
//                  artifact so a hot-path consumer reads bytes, never Calyx.
//
// Honesty gate: Calyx refuses (ungrounded kernel, unmeasured query record, no
// anchored path) with a structured error. That refusal is reported verbatim in
// the tick response and creates NO suggestion. It is never rewritten into a
// low-confidence guess, and never reported as healthy.

/// One composed next-action candidate, before it becomes a suggestion row.
#[derive(Clone, Debug)]
struct NextActionCandidate {
    routine_id: String,
    label: String,
    offer: String,
    confidence: f64,
    grounding: NextActionGrounding,
}

/// The frozen, content-addressed next-action artifact persisted in `CF_KV`.
///
/// Field order is fixed and every member is an explicit value, so the canonical
/// JSON bytes — and therefore `content_sha256` — are deterministic: the
/// fingerprint changes if and only if the composed intelligence changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NextActionArtifact {
    pub record_version: u32,
    pub source_of_truth: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub content_slot: u32,
    pub kernel_id: String,
    pub recall_ratio: f64,
    pub min_recall_ratio: f64,
    pub context_episode_id: String,
    pub context_cx_id: String,
    pub produced_ts_ns: u64,
    pub candidates: Vec<NextActionGrounding>,
    /// sha256 over the canonical bytes of every field above. Self-describing:
    /// a reader recomputes it and refuses a torn or edited artifact.
    pub content_sha256: String,
}

/// The `CF_KV` pointer naming the artifact a reader should load.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NextActionArtifactPointer {
    pub record_version: u32,
    pub content_sha256: String,
    pub produced_ts_ns: u64,
    pub frozen_key: String,
}

/// What one composition pass actually did, reported verbatim on every tick.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NextActionCompositionReport {
    pub source_of_truth: String,
    /// `disabled` | `frozen_artifact` | `calyx_kernel_graph` | `refused`
    pub outcome: String,
    pub grounded: bool,
    pub candidates: u32,
    pub context_episodes_read: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_produced_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_age_secs: Option<u64>,
    pub artifact_staleness_bound_secs: u64,
    /// Kernel nodes whose Base source pointer was not visible at the read
    /// snapshot. Reported, never silently dropped.
    pub unresolved_kernel_nodes: Vec<String>,
    /// Kernel nodes resolving to a source CF the episode composer cannot use.
    pub off_panel_kernel_nodes: Vec<String>,
    /// The exact Calyx/refusal code when nothing was composed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_detail: Option<String>,
}

impl NextActionCompositionReport {
    fn base(outcome: &str, staleness_bound_secs: u64) -> Self {
        Self {
            source_of_truth: NEXT_ACTION_SOURCE_OF_TRUTH.to_owned(),
            outcome: outcome.to_owned(),
            grounded: false,
            candidates: 0,
            context_episodes_read: 0,
            artifact_sha256: None,
            artifact_produced_ts_ns: None,
            artifact_age_secs: None,
            artifact_staleness_bound_secs: staleness_bound_secs,
            unresolved_kernel_nodes: Vec::new(),
            off_panel_kernel_nodes: Vec::new(),
            refusal_code: None,
            refusal_detail: None,
        }
    }

    fn refused(
        staleness_bound_secs: u64,
        code: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let mut report = Self::base("refused", staleness_bound_secs);
        report.refusal_code = Some(code.into());
        report.refusal_detail = Some(detail.into());
        report
    }
}

fn next_action_staleness_secs() -> u64 {
    SuggestionConfig::env_u64(
        "SYNAPSE_ASSIST_NEXT_ACTION_STALENESS_SECS",
        DEFAULT_NEXT_ACTION_STALENESS_SECS,
    )
}

fn next_action_context_lookback_secs(params: &SuggestionTickParams) -> u64 {
    params
        .next_action_context_lookback_secs
        .unwrap_or_else(|| {
            SuggestionConfig::env_u64(
                "SYNAPSE_ASSIST_NEXT_ACTION_CONTEXT_LOOKBACK_SECS",
                DEFAULT_NEXT_ACTION_CONTEXT_LOOKBACK_SECS,
            )
        })
        .clamp(1, MAX_NEXT_ACTION_CONTEXT_LOOKBACK_SECS)
}

fn next_action_max_hops(params: &SuggestionTickParams) -> usize {
    params
        .next_action_max_hops
        .unwrap_or(DEFAULT_NEXT_ACTION_MAX_HOPS)
        .clamp(1, MAX_NEXT_ACTION_MAX_HOPS) as usize
}

fn next_action_frozen_key(content_sha256: &str) -> Vec<u8> {
    format!("{NEXT_ACTION_FROZEN_PREFIX}{content_sha256}").into_bytes()
}

/// Canonical bytes of the artifact with the fingerprint field itself blanked,
/// so `content_sha256` is a hash OF the payload rather than a hash including
/// itself.
fn next_action_artifact_fingerprint(artifact: &NextActionArtifact) -> Result<String, ErrorData> {
    let mut canonical = artifact.clone();
    // Blank every field that CARRIES the fingerprint before hashing. A digest
    // cannot include itself: leaving these populated makes stamping a fixpoint
    // problem with no solution, because each restamp changes the bytes the next
    // digest is taken over.
    canonical.content_sha256 = String::new();
    for candidate in &mut canonical.candidates {
        candidate.artifact_sha256 = String::new();
    }
    let bytes = serde_json::to_vec(&canonical).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("NEXT_ACTION_ARTIFACT_ENCODE_FAILED: {error}"),
        )
    })?;
    Ok(sha256_hex(&bytes))
}

/// Reads the frozen artifact a hot-path consumer would read, verifying the
/// content fingerprint before trusting a single field of it.
pub fn load_next_action_artifact(db: &Arc<Db>) -> Result<Option<NextActionArtifact>, ErrorData> {
    let Some(pointer_value) = load_exact_kv_value(
        db,
        NEXT_ACTION_CURRENT_KEY.as_bytes(),
        "next-action artifact pointer",
    )?
    else {
        return Ok(None);
    };
    let pointer: NextActionArtifactPointer = decode_json(&pointer_value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_POINTER_DECODE_FAILED in CF_KV at {NEXT_ACTION_CURRENT_KEY}: {error}"
            ),
        )
    })?;
    if pointer.record_version != NEXT_ACTION_ARTIFACT_RECORD_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_POINTER_VERSION_UNSUPPORTED: expected {}, got {}",
                NEXT_ACTION_ARTIFACT_RECORD_VERSION, pointer.record_version
            ),
        ));
    }
    let frozen_key = next_action_frozen_key(&pointer.content_sha256);
    let Some(frozen_value) = load_exact_kv_value(db, &frozen_key, "next-action frozen artifact")?
    else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_ARTIFACT_DANGLING: pointer names {} but CF_KV holds no frozen row at {}",
                pointer.content_sha256,
                String::from_utf8_lossy(&frozen_key)
            ),
        ));
    };
    let artifact: NextActionArtifact = decode_json(&frozen_value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_ARTIFACT_DECODE_FAILED in CF_KV at {}: {error}",
                String::from_utf8_lossy(&frozen_key)
            ),
        )
    })?;
    let recomputed = next_action_artifact_fingerprint(&artifact)?;
    if recomputed != artifact.content_sha256 || recomputed != pointer.content_sha256 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_ARTIFACT_FINGERPRINT_MISMATCH: recomputed={recomputed}, artifact={}, pointer={}",
                artifact.content_sha256, pointer.content_sha256
            ),
        ));
    }
    Ok(Some(artifact))
}

fn write_next_action_artifact(
    db: &Arc<Db>,
    artifact: &NextActionArtifact,
) -> Result<(), ErrorData> {
    let frozen_key = next_action_frozen_key(&artifact.content_sha256);
    let frozen_value = encode_json(artifact).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to encode next-action artifact {}: {error}",
                artifact.content_sha256
            ),
        )
    })?;
    let pointer = NextActionArtifactPointer {
        record_version: NEXT_ACTION_ARTIFACT_RECORD_VERSION,
        content_sha256: artifact.content_sha256.clone(),
        produced_ts_ns: artifact.produced_ts_ns,
        frozen_key: String::from_utf8_lossy(&frozen_key).into_owned(),
    };
    let pointer_value = encode_json(&pointer).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("failed to encode next-action artifact pointer: {error}"),
        )
    })?;
    // The frozen row is content-addressed, so re-publishing an unchanged
    // artifact rewrites byte-identical bytes; the pointer swap is what makes a
    // new generation visible, and both land in one batch so a reader never sees
    // a pointer without its artifact.
    db.mutate_batch_pressure_bypass(
        cf::CF_KV,
        Vec::<Vec<u8>>::new(),
        [
            (frozen_key, frozen_value),
            (NEXT_ACTION_CURRENT_KEY.as_bytes().to_vec(), pointer_value),
        ],
    )
    .map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to publish next-action artifact {} atomically: {error}",
                artifact.content_sha256
            ),
        )
    })?;
    let readback = load_next_action_artifact(db)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_ARTIFACT_READBACK_MISSING: {} vanished immediately after publish",
                artifact.content_sha256
            ),
        )
    })?;
    if &readback != artifact {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_ARTIFACT_READBACK_MISMATCH for {}: persisted artifact != value just written",
                artifact.content_sha256
            ),
        ));
    }
    Ok(())
}

/// Resolves one kernel node back to the physical `CF_EPISODES` row it measures.
///
/// `Ok(None)` means the node is genuinely not usable as a next action (no Base
/// pointer visible at this snapshot, or it points at another panel's CF); the
/// caller records that in the report. A pointer that DOES resolve but whose row
/// is missing or undecodable is corruption and fails loud.
fn resolve_kernel_node_episode(
    db: &Arc<Db>,
    cx_id: &str,
    report: &mut NextActionCompositionReport,
) -> Result<Option<(String, EpisodeRecord)>, ErrorData> {
    let pointer = db
        .read_calyx_base_source_pointer(cx_id)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some(pointer) = pointer else {
        report.unresolved_kernel_nodes.push(cx_id.to_owned());
        return Ok(None);
    };
    let (Some(source_cf), Some(source_key_hex)) = (pointer.source_cf, pointer.source_key_hex)
    else {
        report.unresolved_kernel_nodes.push(cx_id.to_owned());
        return Ok(None);
    };
    if source_cf != cf::CF_EPISODES {
        report
            .off_panel_kernel_nodes
            .push(format!("{cx_id}:{source_cf}"));
        return Ok(None);
    }
    let source_key = hex_decode(&source_key_hex).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_SOURCE_KEY_NOT_HEX: Calyx Base row {cx_id} declares source_key_hex={source_key_hex}"
            ),
        )
    })?;
    let Some(source_value) = db
        .get_cf(cf::CF_EPISODES, &source_key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
    else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_SOURCE_ROW_DANGLING: Calyx Base row {cx_id} points at CF_EPISODES key {source_key_hex}, which holds no row"
            ),
        ));
    };
    let (_ts_ns, _ordinal, episode) = decode_episode_row(&source_key, &source_value)?;
    Ok(Some((source_key_hex, episode)))
}

fn next_action_routine_id(context_app: Option<&str>, grounding: &NextActionGrounding) -> String {
    // Stable across ticks for the same observed transition, so the per-routine
    // frequency cap and the #856 decline cooldown both bite on repeats.
    let material = format!(
        "{}\n{}\n{}\n{}",
        context_app.unwrap_or_default(),
        grounding.target_app.as_deref().unwrap_or_default(),
        grounding.target_document.as_deref().unwrap_or_default(),
        grounding.target_url.as_deref().unwrap_or_default(),
    );
    format!(
        "{NEXT_ACTION_ROUTINE_PREFIX}{}",
        sha256_short_hex(&material)
    )
}

fn next_action_label(grounding: &NextActionGrounding) -> String {
    match grounding.target_app.as_deref() {
        Some(app) if !app.trim().is_empty() => format!("Next: {app}"),
        _ => "Next: historically-following step".to_owned(),
    }
}

fn next_action_offer(context_app: Option<&str>, grounding: &NextActionGrounding) -> String {
    let target = grounding
        .target_app
        .as_deref()
        .filter(|app| !app.trim().is_empty())
        .unwrap_or("your next usual step");
    match context_app.filter(|app| !app.trim().is_empty()) {
        Some(context) => format!(
            "After {context} you historically move to {target}. {} kernel-graph hops of grounded evidence support it (confidence {:.2}). Accept to act on it, or dismiss it.",
            grounding.hops.len(),
            grounding.grounded_confidence
        ),
        None => format!(
            "You historically move to {target} next. {} kernel-graph hops of grounded evidence support it (confidence {:.2}). Accept to act on it, or dismiss it.",
            grounding.hops.len(),
            grounding.grounded_confidence
        ),
    }
}

fn candidates_from_artifact(artifact: &NextActionArtifact) -> Vec<NextActionCandidate> {
    artifact
        .candidates
        .iter()
        .map(|grounding| {
            let context_app = grounding.context_app.as_deref();
            NextActionCandidate {
                routine_id: next_action_routine_id(context_app, grounding),
                label: next_action_label(grounding),
                offer: next_action_offer(context_app, grounding),
                confidence: grounding.grounded_confidence,
                grounding: grounding.clone(),
            }
        })
        .collect()
}

/// Composes next actions from the Calyx kernel + between-record graph and
/// lowers them into the frozen artifact.
fn compose_next_actions_from_calyx(
    db: &Arc<Db>,
    now: u64,
    params: &SuggestionTickParams,
    staleness_bound_secs: u64,
) -> Result<(NextActionCompositionReport, Vec<NextActionCandidate>), ErrorData> {
    let mut report = NextActionCompositionReport::base("calyx_kernel_graph", staleness_bound_secs);
    let lookback_ns = next_action_context_lookback_secs(params).saturating_mul(1_000_000_000);
    let context_rows = recent_episode_rows(
        db,
        now.saturating_sub(lookback_ns),
        now,
        NEXT_ACTION_CONTEXT_ROWS,
    )?;
    report.context_episodes_read = u32::try_from(context_rows.len()).unwrap_or(u32::MAX);
    let Some((context_key, context_value, context_episode)) = context_rows.first() else {
        return Ok((
            NextActionCompositionReport::refused(
                staleness_bound_secs,
                "ASSIST_NEXT_ACTION_NO_CONTEXT_EPISODE",
                format!(
                    "no CF_EPISODES row started within the last {} seconds; there is no atomic context measurement to compose from",
                    next_action_context_lookback_secs(params)
                ),
            ),
            Vec::new(),
        ));
    };

    let Some(content_slot) = Db::kernel_content_slot_for_panel(SYN_EPISODE_PANEL_VERSION) else {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "NEXT_ACTION_PANEL_HAS_NO_KERNEL_LANE: panel {SYN_EPISODE_PANEL_VERSION} declares no cold kernel content slot; assist cannot compose next actions off a lane the scheduler does not rebuild"
            ),
        ));
    };

    // ① the atomic context measurement: the episode's own native constellation.
    let context_constellation = db
        .put_episode_constellation(context_key, context_value, context_episode)
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "NEXT_ACTION_CONTEXT_MEASUREMENT_FAILED for episode {}: {error}",
                    context_episode.episode_id
                ),
            )
        })?;

    // ②–④ associate, differentiate, distill: Calyx's kernel + graph, not ours.
    let kernel_params =
        synapse_calyx::SynapseCalyxKernelParams::new(SYN_EPISODE_PANEL_VERSION, content_slot);
    let answer = match db.kernel_answer_intelligence(
        &kernel_params,
        &context_constellation.cx_id,
        next_action_max_hops(params),
    ) {
        Ok(answer) => answer,
        Err(error) => {
            // The honesty gate fired. Report the refusal exactly as Calyx
            // phrased it and surface ZERO suggestions — never a downgraded
            // guess, never a healthy-looking empty result.
            let code = error.code();
            let detail = error.to_string();
            tracing::warn!(
                code = "ASSIST_NEXT_ACTION_REFUSED",
                refusal_code = code,
                panel_version = SYN_EPISODE_PANEL_VERSION,
                content_slot,
                context_cx_id = %context_constellation.cx_id,
                context_episode_id = %context_episode.episode_id,
                detail = %detail,
                "Calyx refused to compose next actions; assist stays silent"
            );
            let mut refused =
                NextActionCompositionReport::refused(staleness_bound_secs, code, detail);
            refused.context_episodes_read = report.context_episodes_read;
            return Ok((refused, Vec::new()));
        }
    };
    if answer.hops.is_empty() {
        let mut refused = NextActionCompositionReport::refused(
            staleness_bound_secs,
            "ASSIST_NEXT_ACTION_NO_HOP_EVIDENCE",
            format!(
                "kernel {} answered from context {} with zero graph hops; there is no between-record evidence to propose a next action from",
                answer.kernel_id, context_constellation.cx_id
            ),
        );
        refused.context_episodes_read = report.context_episodes_read;
        return Ok((refused, Vec::new()));
    }

    let recall_ratio = f64::from(answer.recall_ratio).clamp(0.0, 1.0);
    let context_app = context_episode.app.clone();
    let hops: Vec<NextActionHop> = answer
        .hops
        .iter()
        .map(|hop| NextActionHop {
            from_cx_id: hop.from.clone(),
            to_cx_id: hop.to.clone(),
            edge_weight: f64::from(hop.edge_weight),
            hop_index: hop.hop_index,
            hop_score: f64::from(hop.hop_score),
        })
        .collect();

    let mut groundings: Vec<NextActionGrounding> = Vec::new();
    let mut seen_targets: BTreeSet<String> = BTreeSet::new();
    for (index, hop) in answer.hops.iter().enumerate() {
        let Some((target_source_key_hex, target_episode)) =
            resolve_kernel_node_episode(db, &hop.to, &mut report)?
        else {
            continue;
        };
        // A hop back onto the context itself is not a next action.
        if target_episode.episode_id == context_episode.episode_id {
            continue;
        }
        let dedup_key = format!(
            "{}\u{1f}{}\u{1f}{}",
            target_episode.app.as_deref().unwrap_or_default(),
            target_episode.document.as_deref().unwrap_or_default(),
            target_episode.url.as_deref().unwrap_or_default(),
        );
        if !seen_targets.insert(dedup_key) {
            continue;
        }
        let grounded_confidence =
            (f64::from(hop.hop_score).clamp(0.0, 1.0) * recall_ratio).clamp(0.0, 1.0);
        groundings.push(NextActionGrounding {
            source_of_truth: NEXT_ACTION_SOURCE_OF_TRUTH.to_owned(),
            panel_name: SYN_EPISODE_PANEL_NAME.to_owned(),
            panel_version: answer.panel_version,
            content_slot: u32::from(answer.content_slot),
            kernel_id: answer.kernel_id.clone(),
            kernel_members: answer.kernel_members as u64,
            anchor_kernel_node: answer.anchor_kernel_node.clone(),
            recall_ratio,
            min_recall_ratio: f64::from(answer.min_recall_ratio),
            total_score: f64::from(answer.total_score),
            context_episode_id: context_episode.episode_id.clone(),
            context_episode_key_hex: hex_encode(context_key),
            context_cx_id: context_constellation.cx_id.clone(),
            context_app: context_app.clone(),
            context_start_ts_ns: context_episode.start_ts_ns,
            // Every hop up to and including this one is the evidence path.
            hops: hops.iter().take(index + 1).cloned().collect(),
            target_cx_id: hop.to.clone(),
            target_source_cf: cf::CF_EPISODES.to_owned(),
            target_source_key_hex,
            target_episode_id: target_episode.episode_id.clone(),
            target_app: target_episode.app.clone(),
            target_document: target_episode.document.clone(),
            target_url: target_episode.url.clone(),
            grounded_confidence,
            // Filled once the artifact fingerprint exists.
            artifact_sha256: String::new(),
        });
        if groundings.len() >= MAX_NEXT_ACTION_CANDIDATES {
            break;
        }
    }

    if groundings.is_empty() {
        let mut refused = NextActionCompositionReport::refused(
            staleness_bound_secs,
            "ASSIST_NEXT_ACTION_NO_RESOLVABLE_TARGET",
            format!(
                "kernel {} produced {} hop(s) from context {}, but none resolved to a distinct CF_EPISODES next action (unresolved={}, off_panel={})",
                answer.kernel_id,
                answer.hops.len(),
                context_constellation.cx_id,
                report.unresolved_kernel_nodes.len(),
                report.off_panel_kernel_nodes.len()
            ),
        );
        refused.context_episodes_read = report.context_episodes_read;
        refused.unresolved_kernel_nodes = report.unresolved_kernel_nodes;
        refused.off_panel_kernel_nodes = report.off_panel_kernel_nodes;
        return Ok((refused, Vec::new()));
    }

    // ⑧ lower: freeze the composed intelligence behind a content fingerprint.
    let mut artifact = NextActionArtifact {
        record_version: NEXT_ACTION_ARTIFACT_RECORD_VERSION,
        source_of_truth: NEXT_ACTION_SOURCE_OF_TRUTH.to_owned(),
        panel_name: SYN_EPISODE_PANEL_NAME.to_owned(),
        panel_version: answer.panel_version,
        content_slot: u32::from(answer.content_slot),
        kernel_id: answer.kernel_id.clone(),
        recall_ratio,
        min_recall_ratio: f64::from(answer.min_recall_ratio),
        context_episode_id: context_episode.episode_id.clone(),
        context_cx_id: context_constellation.cx_id,
        produced_ts_ns: now,
        candidates: groundings,
        content_sha256: String::new(),
    };
    let fingerprint = next_action_artifact_fingerprint(&artifact)?;
    artifact.content_sha256.clone_from(&fingerprint);
    for grounding in &mut artifact.candidates {
        grounding.artifact_sha256.clone_from(&fingerprint);
    }
    // Stamping is idempotent by construction (the stamped fields are excluded
    // from the digest input), so this is a real check, not a formality: it fails
    // loud if that invariant is ever broken.
    let restamped = next_action_artifact_fingerprint(&artifact)?;
    if restamped != artifact.content_sha256 {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "NEXT_ACTION_ARTIFACT_FINGERPRINT_UNSTABLE: restamping changed the digest from {} to {restamped}",
                artifact.content_sha256
            ),
        ));
    }
    write_next_action_artifact(db, &artifact)?;

    report.grounded = true;
    report.artifact_sha256 = Some(artifact.content_sha256.clone());
    report.artifact_produced_ts_ns = Some(artifact.produced_ts_ns);
    report.artifact_age_secs = Some(0);
    let candidates = candidates_from_artifact(&artifact);
    report.candidates = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
    tracing::info!(
        code = "ASSIST_NEXT_ACTION_COMPOSED",
        kernel_id = %artifact.kernel_id,
        panel_version = artifact.panel_version,
        content_slot = artifact.content_slot,
        recall_ratio = artifact.recall_ratio,
        context_cx_id = %artifact.context_cx_id,
        candidates = report.candidates,
        artifact_sha256 = %artifact.content_sha256,
        "next actions composed from the Calyx kernel/graph and lowered into a frozen artifact"
    );
    Ok((report, candidates))
}

/// Produces next-action candidates for one tick, recording what the pass did
/// where `health` can read it (#2068 clause 5).
///
/// The recording is the whole point of the wrapper: the composition report
/// otherwise reaches only the caller of `suggestion_tick`, so an operator could
/// learn whether assist can compose only by asking it to compose.
fn next_action_candidates(
    db: &Arc<Db>,
    now: u64,
    params: &SuggestionTickParams,
) -> Result<(NextActionCompositionReport, Vec<NextActionCandidate>), ErrorData> {
    let outcome = compose_next_action_candidates(db, now, params);
    match &outcome {
        Ok((report, _candidates)) => record_next_action_composition(report),
        Err(error) => record_next_action_composition_error(error),
    }
    outcome
}

/// Produces next-action candidates for one tick, preferring the frozen artifact
/// so the common path issues no live Calyx call at all.
fn compose_next_action_candidates(
    db: &Arc<Db>,
    now: u64,
    params: &SuggestionTickParams,
) -> Result<(NextActionCompositionReport, Vec<NextActionCandidate>), ErrorData> {
    let staleness_bound_secs = next_action_staleness_secs();
    if !params.include_next_actions {
        return Ok((
            NextActionCompositionReport::base("disabled", staleness_bound_secs),
            Vec::new(),
        ));
    }
    let existing = load_next_action_artifact(db)?;
    if let Some(artifact) = &existing {
        let age_secs = now.saturating_sub(artifact.produced_ts_ns) / 1_000_000_000;
        let fresh = age_secs <= staleness_bound_secs;
        if fresh && !params.refresh_next_actions {
            let candidates = candidates_from_artifact(artifact);
            let mut report =
                NextActionCompositionReport::base("frozen_artifact", staleness_bound_secs);
            report.grounded = !candidates.is_empty();
            report.candidates = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
            report.artifact_sha256 = Some(artifact.content_sha256.clone());
            report.artifact_produced_ts_ns = Some(artifact.produced_ts_ns);
            report.artifact_age_secs = Some(age_secs);
            return Ok((report, candidates));
        }
    }
    if params.dry_run {
        // Recomposition measures and publishes; a dry run must not. Say so
        // instead of pretending there is nothing to compose.
        let mut report = NextActionCompositionReport::refused(
            staleness_bound_secs,
            "ASSIST_NEXT_ACTION_DRY_RUN_ARTIFACT_STALE",
            match &existing {
                Some(artifact) => format!(
                    "frozen artifact {} is older than the {staleness_bound_secs}s bound and dry_run must not measure or publish; re-run without dry_run to recompose",
                    artifact.content_sha256
                ),
                None => "no frozen next-action artifact exists and dry_run must not measure or publish; re-run without dry_run to compose one".to_owned(),
            },
        );
        report.artifact_sha256 = existing.as_ref().map(|a| a.content_sha256.clone());
        report.artifact_produced_ts_ns = existing.as_ref().map(|a| a.produced_ts_ns);
        return Ok((report, Vec::new()));
    }
    compose_next_actions_from_calyx(db, now, params, staleness_bound_secs)
}

// === Next-action readiness as a health subsystem (#2068 clause 5) ===
//
// Composition state that never leaves the tick response is state an unattended
// operator cannot act on. #2076 is the exact case this exists for: the composer
// refuses every live pass with `ASSIST_NEXT_ACTION_NO_HOP_EVIDENCE` while a
// frozen artifact keeps answering inside its staleness bound, so from outside
// the surface looks merely quiet. The refusal is retained here independently of
// the last outcome precisely so a serving artifact cannot hide it.
//
// Durable truth is still `CF_KV`: the artifact half of this reading is a
// point-read of `assist_next_action/v1/current` and its frozen row on every
// health call, never a cached copy. Only the *composition* half — which pass
// last ran and what it decided — is daemon-generation memory, because there is
// no durable record of a refusal to read (a refusal by construction writes
// nothing).

/// Truncation bound on the refusal detail carried into `health`, so one verbose
/// Calyx refusal cannot dominate the health payload.
const NEXT_ACTION_HEALTH_DETAIL_MAX: usize = 512;

/// Last next-action composition outcome, for `health` (#2068 clause 5).
static LAST_NEXT_ACTION_COMPOSITION: OnceLock<Mutex<NextActionCompositionState>> = OnceLock::new();

#[derive(Debug, Default)]
struct NextActionCompositionState {
    /// `disabled` | `frozen_artifact` | `calyx_kernel_graph` | `refused` | `error`.
    outcome: Option<String>,
    at_unix_ms: Option<u64>,
    grounded: Option<bool>,
    candidates: Option<u32>,
    /// When live Calyx composition last actually succeeded. Retained across
    /// later refusals so a refusal streak cannot hide how old the last real
    /// composition is.
    last_composed_unix_ms: Option<u64>,
    /// Retained across later successes for the same reason in the other
    /// direction.
    refusal_code: Option<String>,
    refusal_detail: Option<String>,
    refusal_at_unix_ms: Option<u64>,
    /// Passes that refused or errored since the last live composition.
    consecutive_refusals: u32,
}

fn next_action_composition_state() -> &'static Mutex<NextActionCompositionState> {
    LAST_NEXT_ACTION_COMPOSITION.get_or_init(|| Mutex::new(NextActionCompositionState::default()))
}

fn with_next_action_composition_state(update: impl FnOnce(&mut NextActionCompositionState)) {
    match next_action_composition_state().lock() {
        Ok(mut state) => update(&mut state),
        Err(error) => tracing::error!(
            code = "ASSIST_NEXT_ACTION_HEALTH_STATE_POISONED",
            error = %error,
            remediation = "inspect daemon logs for a panic inside the next-action composer; the health readback for assist next-action composition is stale until the daemon restarts",
            "next-action composition could not record its outcome for health"
        ),
    }
}

fn next_action_health_detail(detail: &str) -> String {
    if detail.len() <= NEXT_ACTION_HEALTH_DETAIL_MAX {
        return detail.to_owned();
    }
    let mut end = NEXT_ACTION_HEALTH_DETAIL_MAX;
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} […truncated]", &detail[..end])
}

fn record_next_action_composition(report: &NextActionCompositionReport) {
    let at_unix_ms = now_ts_ns() / 1_000_000;
    with_next_action_composition_state(|state| {
        state.outcome = Some(report.outcome.clone());
        state.at_unix_ms = Some(at_unix_ms);
        state.grounded = Some(report.grounded);
        state.candidates = Some(report.candidates);
        match report.outcome.as_str() {
            "refused" => {
                state.refusal_code = report.refusal_code.clone();
                state.refusal_detail = report
                    .refusal_detail
                    .as_deref()
                    .map(next_action_health_detail);
                state.refusal_at_unix_ms = Some(at_unix_ms);
                state.consecutive_refusals = state.consecutive_refusals.saturating_add(1);
            }
            "calyx_kernel_graph" => {
                state.last_composed_unix_ms = Some(at_unix_ms);
                state.consecutive_refusals = 0;
            }
            // `frozen_artifact` served without composing and `disabled` never
            // tried: neither is evidence about the composer, so neither clears a
            // refusal streak nor extends it.
            _ => {}
        }
    });
}

/// Records a composition pass that failed hard rather than refusing honestly.
///
/// An error is not a refusal — it is not the honesty gate declining for want of
/// evidence — but it has the same consequence for the operator (nothing can be
/// composed), so it extends the same streak under its own `error` outcome.
fn record_next_action_composition_error(error: &ErrorData) {
    let at_unix_ms = now_ts_ns() / 1_000_000;
    let code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("ASSIST_NEXT_ACTION_COMPOSITION_FAILED")
        .to_owned();
    let detail = next_action_health_detail(error.message.as_ref());
    with_next_action_composition_state(|state| {
        state.outcome = Some("error".to_owned());
        state.at_unix_ms = Some(at_unix_ms);
        state.grounded = Some(false);
        state.candidates = Some(0);
        state.refusal_code = Some(code.clone());
        state.refusal_detail = Some(detail.clone());
        state.refusal_at_unix_ms = Some(at_unix_ms);
        state.consecutive_refusals = state.consecutive_refusals.saturating_add(1);
    });
}

/// The composition half of [`next_action_health`] alone, for probes that could
/// not reach the `Db` handle (#2087).
///
/// The composition state is process-static and guarded by its own lock, so an
/// M3 state-lock miss is no reason to discard it. Status is `busy` — the
/// artifact readback was skipped because the M3 lock was held by in-flight
/// work, which is contention, not failure; the 21 typed composition fields are
/// still published so the last outcome/refusal stays visible through the miss.
#[must_use]
pub fn next_action_health_m3_lock_busy() -> SubsystemHealth {
    let mut health = next_action_health(None);
    // `None` above set `disabled` / "vault is not open", which is not what
    // happened here — the vault is open, this probe just could not borrow it.
    health.status = "busy".to_owned();
    health.detail = Some(
        "M3 state lock is held by in-flight work; the frozen-artifact readback was skipped for \
         this probe (composition state above is current) — retry the health call for the full \
         reading"
            .to_owned(),
    );
    health
}

/// Reports whether the assist surface can currently compose a next action.
///
/// Read-only by construction: it point-reads the artifact pointer row and the
/// frozen row it names, and never measures, composes, or publishes. A health
/// call that could trigger composition would be a health call that changes the
/// thing it reports.
///
/// `error` is reserved for the two states that mean the surface is broken
/// rather than merely quiet: a `CF_KV` read that fails or returns a torn/dangling
/// artifact, and a hard composition failure that left no artifact behind. An
/// absent artifact because nothing has ever composed is `pending`, and a
/// present-but-stale artifact is its own `stale` state — neither is a fault.
#[must_use]
pub fn next_action_health(db: Option<&Arc<Db>>) -> SubsystemHealth {
    let staleness_bound_secs = next_action_staleness_secs();
    let state = match next_action_composition_state().try_lock() {
        Ok(state) => NextActionCompositionState {
            outcome: state.outcome.clone(),
            at_unix_ms: state.at_unix_ms,
            grounded: state.grounded,
            candidates: state.candidates,
            last_composed_unix_ms: state.last_composed_unix_ms,
            refusal_code: state.refusal_code.clone(),
            refusal_detail: state.refusal_detail.clone(),
            refusal_at_unix_ms: state.refusal_at_unix_ms,
            consecutive_refusals: state.consecutive_refusals,
        },
        Err(std::sync::TryLockError::WouldBlock) => {
            // #2087: a busy composition lock means a composition pass is
            // running at this instant — activity, not failure. Typing it
            // `error` made a contended probe `health.ok`-fatal and
            // indistinguishable from the composer being broken. `busy` is
            // truthful: this probe declined to wait, the next one will read a
            // newer state. Poisoned (below) stays `error` — that is a panic
            // inside the composer, not contention.
            return SubsystemHealth {
                status: "busy".to_owned(),
                detail: Some(
                    "next-action composition state lock is held by an in-flight composition \
                     pass; this probe does not wait behind it — retry the health call for the \
                     settled reading"
                        .to_owned(),
                ),
                ..SubsystemHealth::default()
            };
        }
        Err(std::sync::TryLockError::Poisoned(_error)) => {
            return SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(
                    "next-action composition state lock poisoned by a panic inside the composer"
                        .to_owned(),
                ),
                ..SubsystemHealth::default()
            };
        }
    };

    // Every reading carries the composition half, including the failure
    // readings below: a refusal that is invisible whenever the artifact read
    // also fails would be hidden by exactly the condition it explains.
    let mut health = SubsystemHealth {
        assist_next_action_staleness_bound_secs: Some(staleness_bound_secs),
        assist_next_action_last_composition_outcome: state.outcome.clone(),
        assist_next_action_last_composition_unix_ms: state.at_unix_ms,
        assist_next_action_last_composition_grounded: state.grounded,
        assist_next_action_last_composition_candidates: state.candidates,
        assist_next_action_last_composed_unix_ms: state.last_composed_unix_ms,
        assist_next_action_last_refusal_code: state.refusal_code.clone(),
        assist_next_action_last_refusal_detail: state.refusal_detail.clone(),
        assist_next_action_last_refusal_unix_ms: state.refusal_at_unix_ms,
        assist_next_action_consecutive_refusals: Some(state.consecutive_refusals),
        ..SubsystemHealth::default()
    };

    let Some(db) = db else {
        health.status = "disabled".to_owned();
        health.detail = Some(
            "the vault is not open, so the frozen next-action artifact has no source of truth"
                .to_owned(),
        );
        return health;
    };
    let artifact = match load_next_action_artifact(db) {
        Ok(artifact) => artifact,
        Err(error) => {
            health.status = "error".to_owned();
            health.assist_next_action_artifact_present = Some(false);
            health.detail = Some(format!(
                "reading {NEXT_ACTION_CURRENT_KEY} from CF_KV failed closed: {}",
                next_action_health_detail(error.message.as_ref())
            ));
            return health;
        }
    };

    let composer_blocked = state.consecutive_refusals > 0;
    let refusal_suffix = if composer_blocked {
        format!(
            " last_refusal={} after {} consecutive non-composing passes: {}",
            state.refusal_code.as_deref().unwrap_or("<unnamed>"),
            state.consecutive_refusals,
            state.refusal_detail.as_deref().unwrap_or("<none>"),
        )
    } else {
        String::new()
    };

    let Some(artifact) = artifact else {
        health.assist_next_action_artifact_present = Some(false);
        let (status, detail) = match state.outcome.as_deref() {
            None => (
                "pending",
                format!(
                    "no frozen next-action artifact exists at {NEXT_ACTION_CURRENT_KEY} and no \
                     composition pass has run in this daemon generation, so assist has not yet \
                     had the chance to compose; run assist operation=suggestion_tick with \
                     include_next_actions"
                ),
            ),
            Some("error") => (
                "error",
                format!(
                    "no frozen next-action artifact exists and the last composition pass failed \
                     hard, so assist cannot compose:{refusal_suffix}"
                ),
            ),
            Some(_) if composer_blocked => (
                "refused",
                format!(
                    "no frozen next-action artifact exists and the composer is refusing on the \
                     honesty gate, so assist correctly composes nothing:{refusal_suffix}"
                ),
            ),
            Some(outcome) => (
                "pending",
                format!(
                    "no frozen next-action artifact exists; the last composition pass was \
                     outcome={outcome}, which published nothing"
                ),
            ),
        };
        health.status = status.to_owned();
        health.detail = Some(detail);
        return health;
    };

    let age_secs = now_ts_ns().saturating_sub(artifact.produced_ts_ns) / 1_000_000_000;
    let stale = age_secs > staleness_bound_secs;
    health.assist_next_action_artifact_present = Some(true);
    health.assist_next_action_artifact_sha256 = Some(artifact.content_sha256.clone());
    health.assist_next_action_artifact_built_at_unix_ms = Some(artifact.produced_ts_ns / 1_000_000);
    health.assist_next_action_artifact_age_secs = Some(age_secs);
    health.assist_next_action_artifact_stale = Some(stale);
    health.assist_next_action_artifact_candidates =
        Some(u32::try_from(artifact.candidates.len()).unwrap_or(u32::MAX));
    health.assist_next_action_artifact_kernel_id = Some(artifact.kernel_id.clone());
    health.assist_next_action_artifact_panel_name = Some(artifact.panel_name.clone());
    health.assist_next_action_artifact_panel_version = Some(artifact.panel_version);
    health.assist_next_action_artifact_content_slot = Some(artifact.content_slot);
    health.assist_next_action_artifact_recall_ratio = Some(artifact.recall_ratio);
    health.assist_next_action_artifact_min_recall_ratio = Some(artifact.min_recall_ratio);

    let status = match (stale, composer_blocked) {
        // Present, inside its bound, and nothing is refusing underneath it.
        (false, false) => "ok",
        // Serving, but only because the frozen artifact has not expired yet:
        // the composer that would replace it is not producing (#2076).
        (false, true) => "degraded",
        // Past the bound, so the next non-dry tick recomposes rather than
        // serving it. Visible in its own right, and not a fault.
        (true, _) => "stale",
    };
    health.status = status.to_owned();
    health.detail = Some(format!(
        "frozen artifact {} built {} s ago (bound {staleness_bound_secs} s, stale={stale}) with {} \
         candidates from kernel {} over {}@{} slot {} at recall {:.4} vs gate {:.4}; last \
         composition outcome={}{refusal_suffix}",
        artifact.content_sha256,
        age_secs,
        artifact.candidates.len(),
        artifact.kernel_id,
        artifact.panel_name,
        artifact.panel_version,
        artifact.content_slot,
        artifact.recall_ratio,
        artifact.min_recall_ratio,
        state
            .outcome
            .as_deref()
            .unwrap_or("<none in this generation>"),
    ));
    health
}

fn build_next_action_suggestion(
    candidate: &NextActionCandidate,
    now: u64,
    config: &SuggestionConfig,
) -> SuggestionRecord {
    SuggestionRecord {
        record_version: SUGGESTION_RECORD_VERSION,
        suggestion_id: format!("sg1-{}-{now:020}", candidate.routine_id),
        routine_id: candidate.routine_id.clone(),
        source: SuggestionSource::KernelNextAction,
        source_event_id: Some(candidate.grounding.target_cx_id.clone()),
        label: Some(candidate.label.clone()),
        offer: Some(candidate.offer.clone()),
        mitigation: None,
        next_action: Some(candidate.grounding.clone()),
        created_ts_ns: now,
        expiry_ts_ns: now.saturating_add(config.expiry_secs.saturating_mul(1_000_000_000)),
        status: SuggestionStatus::Live,
        confidence: candidate.confidence,
        matched_prefix_len: 1,
        total_steps: 1,
        remaining_step_count: 1,
        proposed_plan_ref: None,
        resolved_ts_ns: None,
        resolution_note: None,
    }
}

/// The plan for a composed next action.
///
/// Deliberately a single `AgentTask` step: the composition proves *what* the
/// operator historically does next, not that opening it blindly is correct. The
/// executor refuses an `AgentTask` step with its reason, which surfaces the full
/// grounding to the caller instead of launching something unverified.
pub fn next_action_plan_for_suggestion(
    record: &SuggestionRecord,
    compiled_ts_ns: u64,
) -> Result<PlanDocument, ErrorData> {
    if record.source != SuggestionSource::KernelNextAction {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "NEXT_ACTION_PLAN_SOURCE_MISMATCH: suggestion {} has source {:?}",
                record.suggestion_id, record.source
            ),
        ));
    }
    let Some(grounding) = &record.next_action else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "NEXT_ACTION_SUGGESTION_MISSING_GROUNDING: suggestion {} claims kernel/graph composition but carries no evidence",
                record.suggestion_id
            ),
        ));
    };
    let target_app = grounding
        .target_app
        .clone()
        .unwrap_or_else(|| "kernel-next-action".to_owned());
    Ok(PlanDocument {
        record_version: ASSIST_PLAN_RECORD_VERSION,
        routine_id: record.routine_id.clone(),
        compiled_ts_ns,
        granularity: RoutineGranularity::App,
        schedule_label: "kernel next action".to_owned(),
        total_steps: 1,
        deterministic_steps: 0,
        agent_task_steps: 1,
        fully_deterministic: false,
        steps: vec![PlanStep {
            index: 0,
            source_app: target_app.clone(),
            source_document: grounding.target_document.clone(),
            backend: PlanBackend::AgentTask,
            deterministic: false,
            action: format!(
                "act on the composed next action {target_app} for suggestion {}",
                record.suggestion_id
            ),
            postcondition: Postcondition::AgentReported,
            agent_task_reason: Some(format!(
                "Composed from Calyx kernel {} on panel {} slot {} (recall {:.4} >= gate {:.4}) walking {} graph hop(s) from context episode {} ({}) to episode {} ({}). Grounded confidence {:.4}; frozen artifact {}. Verify the operator actually wants this step before acting: the evidence proves what historically followed, not that it is correct now.",
                grounding.kernel_id,
                grounding.panel_version,
                grounding.content_slot,
                grounding.recall_ratio,
                grounding.min_recall_ratio,
                grounding.hops.len(),
                grounding.context_episode_id,
                grounding.context_app.as_deref().unwrap_or("unknown app"),
                grounding.target_episode_id,
                target_app,
                grounding.grounded_confidence,
                grounding.artifact_sha256,
            )),
        }],
    })
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
    db.get_cf(cf::CF_KV, key).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "SUGGESTION_EXACT_KEY_READ_FAILED: {context} key {} in CF_KV: {error}",
                String::from_utf8_lossy(key)
            ),
        )
    })
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

/// Persists one suggestion row and returns the exact key and the exact
/// persisted bytes. The bytes are load-bearing: a Calyx constellation id is
/// derived from the source value, so an anchor built from a re-encoded record
/// would name a different `cx_id` than the row storage actually holds.
fn write_suggestion(
    db: &Arc<Db>,
    record: &SuggestionRecord,
) -> Result<(Vec<u8>, Vec<u8>), ErrorData> {
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
    )?;
    Ok((key, primary_readback_value))
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

// === Grounded accept/decline outcome anchors (#2046, #1690 clause 3) ===
//
// "operator accept/dismiss of suggestions written back as anchors (the steering
// loop grounds itself)."
//
// Two anchor lanes, and the second is the one that makes steering learn:
//
//  1. the OFFER lane — an outcome anchor on the exact `CF_KV suggestion/v1` row
//     (the durable row IS the identity of the decision, so a later reader can
//     tie the anchor back to one suggestion id and nothing else);
//  2. the EVIDENCE lane — for a kernel/graph-composed suggestion, the same
//     decision is anchored onto the `CF_EPISODES` rows that PRODUCED it (the
//     context episode and the proposed target episode). Recommender practice is
//     unambiguous here: an explicit dismissal has to attach to the generating
//     evidence, not only to the impression, or the next composition proposes
//     the same thing again from the same untouched edges.
//
// Both lanes go through the shared `m3::grounding` helpers, so the ledger
// payload is byte-identical to every other Synapse anchor of the same shape.

/// One evidence-lane anchor, independently rescanned from the Anchors CF.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionEvidenceAnchorReport {
    pub anchor_kind: String,
    pub source_cf: String,
    pub source_key_hex: String,
    pub episode_id: String,
    pub cx_id: String,
    /// Provenance-ledger sequence of the write that created this anchor.
    /// Absent on an idempotent replay: the replay lane rescans the physical
    /// Anchors CF, which carries the anchor but not the seq of the commit that
    /// first stamped it. Reporting a placeholder there would be a lie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_seq: Option<u64>,
    pub readback_exact_match_count: u64,
}

/// The grounded anchor written for one accept/decline decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionOutcomeAnchorReport {
    pub source_of_truth: String,
    pub suggestion_id: String,
    /// `accepted` or `declined` — the anchor's enum value, verbatim.
    pub decision: String,
    pub anchor_kind: String,
    pub source_cf: String,
    pub source_key_hex: String,
    pub source_value_sha256: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub cx_id: String,
    /// Provenance-ledger sequence/hash of the write that created this anchor.
    /// Both are absent on an idempotent replay — see
    /// [`SuggestionEvidenceAnchorReport::ledger_seq`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_hash: Option<String>,
    /// Anchors on this constellation after the write, read back from the
    /// physical Anchors CF by a second, independent scan.
    pub readback_anchor_count: u64,
    /// How many of those match this exact kind AND value.
    pub readback_exact_match_count: u64,
    pub evidence_anchors: Vec<SuggestionEvidenceAnchorReport>,
    /// Evidence rows the anchor could not reach. `CF_EPISODES` is replaceable
    /// derived state, so a re-segmented day legitimately removes a row — that is
    /// reported here, never silently dropped.
    pub missing_evidence_rows: Vec<String>,
    /// Present ONLY when this call completed a known-incomplete terminal write
    /// (see [`SuggestionAnchorRepairReport`]). Absent on every healthy path, so
    /// a caller can tell a normal replay from a repaired one without parsing
    /// log text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_repair: Option<SuggestionAnchorRepairReport>,
}

/// Evidence that a torn terminal resolution was COMPLETED, not papered over.
///
/// A suggestion row that is already `accepted`/`declined` while the physical
/// Anchors CF holds zero matching anchors is the durable footprint of a write
/// that this module itself began and failed to finish (the pre-#2068 decline
/// path flipped the row before grounding it, and a crash between the two writes
/// can still produce it). The row is then unreachable by every transition:
/// `suggestion_accept` refuses it as not-live and `suggestion_decline` takes the
/// replay lane, which demands the anchor that was never written.
///
/// Finishing that write is completion, not a fallback: the anchor is
/// re-derived deterministically from the PERSISTED row (including its original
/// `resolved_ts_ns`), so it is byte-identical to the anchor the interrupted call
/// would have written; the repair is announced in the response and in the
/// daemon record with its own code; and the post-repair rescan still has to
/// return exactly one match or the call fails loud exactly as before. Nothing is
/// inferred, defaulted, or swallowed — the only alternative is a row that errors
/// forever.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionAnchorRepairReport {
    /// Always `SUGGESTION_ANCHOR_REPAIR_COMPLETED`; matches the daemon record.
    pub code: String,
    /// Why the repair was admissible, in full.
    pub reason: String,
    /// Anchors of this exact kind+value found BEFORE the repair. Only `0`
    /// admits a repair — a count above 1 is ambiguity, never incompleteness.
    pub matching_anchors_before: u64,
    /// Anchors of any kind on the constellation before the repair.
    pub anchors_on_constellation_before: u64,
    /// Provenance-ledger sequence/hash of the completing write. Present because
    /// a repair really does write, which is exactly what distinguishes it from
    /// an ordinary replay.
    pub ledger_seq: Option<u64>,
    pub ledger_hash: Option<String>,
}

fn suggestion_decision_label(status: SuggestionStatus) -> Option<&'static str> {
    match status {
        SuggestionStatus::Accepted => Some("accepted"),
        SuggestionStatus::Declined => Some("declined"),
        SuggestionStatus::Live | SuggestionStatus::Expired | SuggestionStatus::Abandoned => None,
    }
}

/// Counts anchors of one kind+value on a source row by a fresh physical scan of
/// the Calyx `Anchors` CF — never by trusting the write report.
fn rescan_anchor_matches(
    db: &Arc<Db>,
    source_cf: &'static str,
    source_key: &[u8],
    source_value: &[u8],
    anchor_kind: &str,
    decision: &str,
) -> Result<(u64, u64), ErrorData> {
    let scan = db
        .calyx_anchor_scan_for_source(source_cf, source_key, source_value)
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "SUGGESTION_ANCHOR_RESCAN_FAILED for {source_cf} key {}: {error}",
                    hex_encode(source_key)
                ),
            )
        })?;
    let total = scan.anchors.len() as u64;
    let exact = scan
        .anchors
        .iter()
        .filter(|row| anchor_row_matches(row, anchor_kind, decision))
        .count() as u64;
    Ok((total, exact))
}

/// How the physical Anchors CF renders a LABELLED anchor kind.
///
/// `grounding::enum_anchor` takes the bare kind (`synapse:suggestion_outcome`)
/// and the storage layer stores it as `AnchorKind::Label(kind)`, which reads
/// back as `label:<kind>` (`synapse-storage/src/backend.rs`,
/// `anchor_kind_label`). Every readback in this module therefore has to compare
/// against the RENDERED form; comparing the bare kind can never match, so the
/// exactly-one-match self-check silently finds nothing and every accept/decline
/// fails after it has already mutated the row (#2068 defect 2). The identical
/// trap was caught during manual outcome-anchor FSV — this helper exists so the
/// rendering is derived in exactly one place here, never
/// re-spelled at a call site.
fn rendered_anchor_kind(anchor_kind: &str) -> String {
    format!("label:{anchor_kind}")
}

/// An anchor matches only when the kind, the value TYPE, and the value all
/// agree. Matching on the text alone would let an unrelated free-text anchor
/// masquerade as the operator's enum decision.
fn anchor_row_matches(
    row: &synapse_storage::CalyxAnchorRow,
    anchor_kind: &str,
    decision: &str,
) -> bool {
    row.kind == rendered_anchor_kind(anchor_kind)
        && row.value.value_type == "enum"
        && row.value.text_value.as_deref() == Some(decision)
}

/// Writes the offer-lane and evidence-lane anchors for one resolved suggestion.
///
/// Idempotent by construction: the anchor is content-addressed by the persisted
/// row's `cx_id` plus its kind/value/source/observed_at, so replaying an
/// unchanged decision re-derives the identical anchor and Calyx's own
/// exactly-one-match readback holds. This is why `resolved_ts_ns` must never be
/// regenerated on a replay — a fresh clock stamp would mint a second anchor for
/// one logical decision.
fn anchor_suggestion_outcome(
    db: &Arc<Db>,
    suggestion_key: &[u8],
    suggestion_value: &[u8],
    record: &SuggestionRecord,
) -> Result<SuggestionOutcomeAnchorReport, ErrorData> {
    let Some(decision) = suggestion_decision_label(record.status) else {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "SUGGESTION_OUTCOME_ANCHOR_NOT_A_DECISION: suggestion {} has status {:?}, which is not an operator decision",
                record.suggestion_id, record.status
            ),
        ));
    };
    let observed_at_ms =
        grounding::observed_at_ms_from_ns(record.resolved_ts_ns.unwrap_or(record.created_ts_ns));
    let record_value = serde_json::to_value(record).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "suggestion outcome anchor failed to project suggestion {}: {error}",
                record.suggestion_id
            ),
        )
    })?;
    let offer = grounding::write_outcome_constellation_and_anchor(
        db,
        cf::CF_KV,
        suggestion_key,
        suggestion_value,
        &record_value,
        grounding::enum_anchor(
            SUGGESTION_OUTCOME_ANCHOR_KIND,
            decision,
            SOURCE_OPERATOR,
            observed_at_ms,
        ),
        "suggestion outcome anchor",
    )?;
    let (readback_anchor_count, readback_exact_match_count) = rescan_anchor_matches(
        db,
        cf::CF_KV,
        suggestion_key,
        suggestion_value,
        SUGGESTION_OUTCOME_ANCHOR_KIND,
        decision,
    )?;
    if readback_exact_match_count != 1 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_OUTCOME_ANCHOR_READBACK_MISMATCH for {}: independent Anchors CF scan of cx_id {} found {readback_exact_match_count} anchors of physical kind {}={decision} (bare kind {SUGGESTION_OUTCOME_ANCHOR_KIND}), expected exactly 1",
                record.suggestion_id,
                offer.cx_id,
                rendered_anchor_kind(SUGGESTION_OUTCOME_ANCHOR_KIND)
            ),
        ));
    }

    let mut evidence_anchors = Vec::new();
    let mut missing_evidence_rows = Vec::new();
    if let Some(grounding_evidence) = &record.next_action {
        let evidence_kind = format!(
            "{NEXT_ACTION_EVIDENCE_ANCHOR_KIND}:{}",
            record.suggestion_id
        );
        for (role, key_hex, episode_id) in [
            (
                "context",
                grounding_evidence.context_episode_key_hex.as_str(),
                grounding_evidence.context_episode_id.as_str(),
            ),
            (
                "target",
                grounding_evidence.target_source_key_hex.as_str(),
                grounding_evidence.target_episode_id.as_str(),
            ),
        ] {
            let Some(episode_key) = hex_decode(key_hex) else {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "SUGGESTION_EVIDENCE_KEY_NOT_HEX: suggestion {} {role} evidence declares key {key_hex}",
                        record.suggestion_id
                    ),
                ));
            };
            let Some(episode_value) = db
                .get_cf(cf::CF_EPISODES, &episode_key)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?
            else {
                missing_evidence_rows.push(format!(
                    "{role}:{episode_id}:CF_EPISODES key {key_hex} no longer holds a row (re-segmented day)"
                ));
                continue;
            };
            let (_ts_ns, _ordinal, episode) = decode_episode_row(&episode_key, &episode_value)?;
            db.put_episode_constellation(&episode_key, &episode_value, &episode)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "suggestion evidence anchor failed to ensure the {role} episode constellation for {}: {error}",
                            episode.episode_id
                        ),
                    )
                })?;
            let report = grounding::write_anchor_for_existing_constellation(
                db,
                cf::CF_EPISODES,
                &episode_key,
                &episode_value,
                grounding::enum_anchor(
                    evidence_kind.clone(),
                    decision,
                    SOURCE_OPERATOR,
                    observed_at_ms,
                ),
                "suggestion next-action evidence anchor",
            )?;
            let (_total, exact) = rescan_anchor_matches(
                db,
                cf::CF_EPISODES,
                &episode_key,
                &episode_value,
                &evidence_kind,
                decision,
            )?;
            evidence_anchors.push(SuggestionEvidenceAnchorReport {
                anchor_kind: evidence_kind.clone(),
                source_cf: cf::CF_EPISODES.to_owned(),
                source_key_hex: key_hex.to_owned(),
                episode_id: episode.episode_id.clone(),
                cx_id: report.cx_id.clone(),
                ledger_seq: Some(report.ledger_seq),
                readback_exact_match_count: exact,
            });
            tracing::info!(
                code = "SUGGESTION_EVIDENCE_ANCHORED",
                suggestion_id = %record.suggestion_id,
                role,
                decision,
                episode_id = %episode.episode_id,
                cx_id = %report.cx_id,
                ledger_seq = report.ledger_seq,
                "suggestion decision grounded on the episode constellation that produced it"
            );
        }
    }

    tracing::info!(
        code = "SUGGESTION_OUTCOME_ANCHORED",
        suggestion_id = %record.suggestion_id,
        routine_id = %record.routine_id,
        source = ?record.source,
        decision,
        source_key_hex = %offer.source_key_hex,
        cx_id = %offer.cx_id,
        ledger_seq = offer.ledger_seq,
        evidence_anchors = evidence_anchors.len(),
        missing_evidence_rows = missing_evidence_rows.len(),
        "suggestion decision grounded as a Calyx outcome anchor"
    );

    Ok(SuggestionOutcomeAnchorReport {
        source_of_truth: SUGGESTION_ANCHOR_SOURCE_OF_TRUTH.to_owned(),
        suggestion_id: record.suggestion_id.clone(),
        decision: decision.to_owned(),
        anchor_kind: SUGGESTION_OUTCOME_ANCHOR_KIND.to_owned(),
        source_cf: offer.source_cf,
        source_key_hex: offer.source_key_hex,
        source_value_sha256: offer.source_value_sha256,
        panel_name: offer.panel_name,
        panel_version: offer.panel_version,
        cx_id: offer.cx_id,
        ledger_seq: Some(offer.ledger_seq),
        ledger_hash: Some(offer.ledger_hash),
        readback_anchor_count,
        readback_exact_match_count,
        evidence_anchors,
        missing_evidence_rows,
        anchor_repair: None,
    })
}

/// Daemon-record and response code announcing a completed torn terminal write.
const SUGGESTION_ANCHOR_REPAIR_CODE: &str = "SUGGESTION_ANCHOR_REPAIR_COMPLETED";

/// Completes the grounding of a terminal suggestion row whose outcome anchor is
/// physically absent — the durable footprint of a resolution that flipped the
/// row and then failed before it could stamp the anchor (#2068 defect 3).
///
/// Admissible only when the rescan found EXACTLY ZERO matching anchors: that is
/// incompleteness. Any other count is ambiguity and is refused above. The
/// completing write re-derives the anchor from the persisted row alone — same
/// kind, same enum value, same `observed_at` (from the row's original
/// `resolved_ts_ns`) — so it is content-identical to the anchor the interrupted
/// call would have written, and a repair that somehow does not land still fails
/// loud through `anchor_suggestion_outcome`'s own exactly-one-match self-check.
fn repair_missing_outcome_anchor(
    db: &Arc<Db>,
    suggestion_key: &[u8],
    suggestion_value: &[u8],
    record: &SuggestionRecord,
    decision: &str,
    scan: &synapse_storage::CalyxAnchorScanReport,
) -> Result<SuggestionOutcomeAnchorReport, ErrorData> {
    let Some(resolved_ts_ns) = record.resolved_ts_ns else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ANCHOR_REPAIR_REFUSED for {}: the row is {decision} but carries no resolved_ts_ns, so the anchor's observed_at cannot be re-derived from the persisted row; this is not a merely-incomplete write and must not be repaired",
                record.suggestion_id
            ),
        ));
    };
    tracing::warn!(
        code = "SUGGESTION_ANCHOR_REPAIR_STARTED",
        suggestion_id = %record.suggestion_id,
        routine_id = %record.routine_id,
        source = ?record.source,
        decision,
        resolved_ts_ns,
        cx_id = %scan.cx_id,
        anchors_on_constellation = scan.anchors.len(),
        "terminal suggestion row has no matching outcome anchor; completing the interrupted grounding write"
    );
    let anchors_on_constellation_before = scan.anchors.len() as u64;
    let scanned_cx_id = scan.cx_id.clone();
    let mut report = anchor_suggestion_outcome(db, suggestion_key, suggestion_value, record)?;
    if report.cx_id != scanned_cx_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ANCHOR_REPAIR_CONSTELLATION_DRIFTED for {}: the scan that found the missing anchor read cx_id {scanned_cx_id}, the completing write landed on cx_id {}; the repair did not land on the row it was diagnosed from",
                record.suggestion_id, report.cx_id
            ),
        ));
    }
    let reason = format!(
        "row {} was already {decision} (resolved_ts_ns={resolved_ts_ns}) while the physical Anchors CF held 0 anchors of kind {} on cx_id {scanned_cx_id}: a terminal write that flipped the durable status and failed before grounding it. The missing anchor was re-derived from the persisted row and written; it is byte-identical to the one the interrupted call would have stamped, and the post-write independent rescan returned exactly 1 match.",
        record.suggestion_id,
        rendered_anchor_kind(SUGGESTION_OUTCOME_ANCHOR_KIND)
    );
    tracing::warn!(
        code = SUGGESTION_ANCHOR_REPAIR_CODE,
        suggestion_id = %record.suggestion_id,
        routine_id = %record.routine_id,
        decision,
        cx_id = %report.cx_id,
        ledger_seq = report.ledger_seq.unwrap_or_default(),
        readback_exact_match_count = report.readback_exact_match_count,
        evidence_anchors = report.evidence_anchors.len(),
        "completed a torn terminal suggestion write; the decision and its grounding agree again"
    );
    report.anchor_repair = Some(SuggestionAnchorRepairReport {
        code: SUGGESTION_ANCHOR_REPAIR_CODE.to_owned(),
        reason,
        matching_anchors_before: 0,
        anchors_on_constellation_before,
        ledger_seq: report.ledger_seq,
        ledger_hash: report.ledger_hash.clone(),
    });
    Ok(report)
}

/// Rebuilds the anchor report for an ALREADY-anchored decision by reading the
/// physical Anchors CF only. Used on an idempotent replay, where re-running the
/// side effect is exactly what must not happen.
///
/// One exception, and it is not a fallback: when the row is terminal and the
/// physical Anchors CF holds ZERO matching anchors, the terminal write is
/// provably incomplete (see [`SuggestionAnchorRepairReport`]) and this lane
/// completes it idempotently instead of erroring forever.
fn read_suggestion_outcome_anchor(
    db: &Arc<Db>,
    suggestion_key: &[u8],
    suggestion_value: &[u8],
    record: &SuggestionRecord,
) -> Result<SuggestionOutcomeAnchorReport, ErrorData> {
    let Some(decision) = suggestion_decision_label(record.status) else {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "SUGGESTION_OUTCOME_ANCHOR_NOT_A_DECISION: suggestion {} has status {:?}",
                record.suggestion_id, record.status
            ),
        ));
    };
    let scan = db
        .calyx_anchor_scan_for_source(cf::CF_KV, suggestion_key, suggestion_value)
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "SUGGESTION_ANCHOR_REPLAY_SCAN_FAILED for {}: {error}",
                    record.suggestion_id
                ),
            )
        })?;
    let exact = scan
        .anchors
        .iter()
        .filter(|row| anchor_row_matches(row, SUGGESTION_OUTCOME_ANCHOR_KIND, decision))
        .count() as u64;
    if exact == 0 {
        // Torn terminal write: the row carries the decision, the grounding was
        // never stamped. Complete it — see `SuggestionAnchorRepairReport` for
        // why this is completion and not a fallback.
        return repair_missing_outcome_anchor(
            db,
            suggestion_key,
            suggestion_value,
            record,
            decision,
            &scan,
        );
    }
    if exact != 1 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_ANCHOR_REPLAY_AMBIGUOUS for {}: the row is already {decision} but the physical Anchors CF holds {exact} anchors of physical kind {}={decision} on cx_id {}; exactly one is required and more than one is ambiguity, not an incomplete write — this is NOT repairable and needs an operator",
                record.suggestion_id,
                rendered_anchor_kind(SUGGESTION_OUTCOME_ANCHOR_KIND),
                scan.cx_id
            ),
        ));
    }
    let mut evidence_anchors = Vec::new();
    let mut missing_evidence_rows = Vec::new();
    if let Some(grounding_evidence) = &record.next_action {
        let evidence_kind = format!(
            "{NEXT_ACTION_EVIDENCE_ANCHOR_KIND}:{}",
            record.suggestion_id
        );
        for (role, key_hex, episode_id) in [
            (
                "context",
                grounding_evidence.context_episode_key_hex.as_str(),
                grounding_evidence.context_episode_id.as_str(),
            ),
            (
                "target",
                grounding_evidence.target_source_key_hex.as_str(),
                grounding_evidence.target_episode_id.as_str(),
            ),
        ] {
            let Some(episode_key) = hex_decode(key_hex) else {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "SUGGESTION_EVIDENCE_KEY_NOT_HEX: suggestion {} {role} evidence declares key {key_hex}",
                        record.suggestion_id
                    ),
                ));
            };
            let Some(episode_value) = db
                .get_cf(cf::CF_EPISODES, &episode_key)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?
            else {
                missing_evidence_rows.push(format!(
                    "{role}:{episode_id}:CF_EPISODES key {key_hex} no longer holds a row (re-segmented day)"
                ));
                continue;
            };
            let evidence_scan = db
                .calyx_anchor_scan_for_source(cf::CF_EPISODES, &episode_key, &episode_value)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            let evidence_exact = evidence_scan
                .anchors
                .iter()
                .filter(|row| anchor_row_matches(row, &evidence_kind, decision))
                .count() as u64;
            evidence_anchors.push(SuggestionEvidenceAnchorReport {
                anchor_kind: evidence_kind.clone(),
                source_cf: cf::CF_EPISODES.to_owned(),
                source_key_hex: key_hex.to_owned(),
                episode_id: episode_id.to_owned(),
                cx_id: evidence_scan.cx_id.clone(),
                ledger_seq: None,
                readback_exact_match_count: evidence_exact,
            });
        }
    }
    Ok(SuggestionOutcomeAnchorReport {
        source_of_truth: SUGGESTION_ANCHOR_SOURCE_OF_TRUTH.to_owned(),
        suggestion_id: record.suggestion_id.clone(),
        decision: decision.to_owned(),
        anchor_kind: SUGGESTION_OUTCOME_ANCHOR_KIND.to_owned(),
        source_cf: scan.source_cf,
        source_key_hex: scan.source_key_hex,
        source_value_sha256: scan.source_value_sha256,
        panel_name: scan.panel_name,
        panel_version: scan.panel_version,
        cx_id: scan.cx_id,
        ledger_seq: None,
        ledger_hash: None,
        readback_anchor_count: scan.anchors.len() as u64,
        readback_exact_match_count: exact,
        evidence_anchors,
        missing_evidence_rows,
        anchor_repair: None,
    })
}

pub fn accept_suggestion_for_execution(
    db: &Arc<Db>,
    suggestion_id: &str,
    now_ns: u64,
    plan_ref: &str,
    execution_id: &str,
    dry_run: bool,
) -> Result<(SuggestionRecord, Option<SuggestionOutcomeAnchorReport>), ErrorData> {
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
    if dry_run {
        // A dry run mutates nothing, so it must not DESCRIBE a mutation. The
        // record is returned exactly as it is persisted — still `live`, no
        // `resolved_ts_ns`, no `proposed_plan_ref`, no resolution note — and the
        // dry-run intent is carried by the execution record's own `dry_run`
        // flag and the absent `outcome_anchor`. Stamping a would-be resolution
        // here made the response claim a state storage never held (#2068).
        tracing::info!(
            code = "SUGGESTION_ACCEPT_DRY_RUN",
            suggestion_id = %record.suggestion_id,
            routine_id = %record.routine_id,
            source = ?record.source,
            status = ?record.status,
            would_be_plan_ref = plan_ref,
            would_be_execution_id = execution_id,
            "suggestion_accept dry run: no CF_KV write, no #856 feedback, no outcome anchor; the returned row is the unresolved persisted row"
        );
        return Ok((record, None));
    }
    let live_record = record.clone();
    record.status = SuggestionStatus::Accepted;
    record.proposed_plan_ref = Some(plan_ref.to_owned());
    record.resolved_ts_ns = Some(now_ns);
    record.resolution_note = Some(format!(
        "accepted by suggestion_accept; execution_id={execution_id}"
    ));
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "suggestion_accept refused under disk pressure: pressure_level={:?}",
                db.pressure_level()
            ),
        ));
    }
    let (key, value) = write_suggestion(db, &record)?;
    // The status flip is only half of one logical decision: the grounding anchor
    // is the other half, and the anchor can only be written against the value
    // that is actually in `CF_KV`. So the flip goes first and any failure after
    // it is compensated — the row goes back to `Live` and the offer stays
    // retryable, instead of being left accepted-without-grounding, which no
    // transition can reach (#2068 defect 3).
    let anchor = match anchor_suggestion_outcome(db, &key, &value, &record) {
        Ok(anchor) => anchor,
        Err(error) => {
            return Err(roll_back_suggestion_resolution(
                db,
                &live_record,
                "suggestion_accept",
                error,
            ));
        }
    };
    Ok((record, Some(anchor)))
}

/// Compensates a durable status flip whose decision could not be completed.
///
/// The `CF_KV` suggestion row and the Calyx anchor cannot be written in one
/// batch — the anchor is content-addressed by the PERSISTED row value, so the
/// row must exist first. That makes the flip a compensable step, not an atomic
/// one, and the compensation is what keeps the invariant the FSV actually needs:
/// **a suggestion is never terminal without its grounding**. On failure the
/// original `Live` record is rewritten, and `write_suggestion` read-back-verifies
/// the persisted row equals it byte-for-byte, so "the row is provably back in
/// `Live`" is a proof, not a claim.
///
/// The original failure is returned with its structured code intact, extended
/// with the rollback evidence. If the rollback itself fails the error escalates
/// to `STORAGE_CORRUPTED` naming both failures — that, and only that, is a state
/// an operator must resolve.
fn roll_back_suggestion_resolution(
    db: &Arc<Db>,
    live_record: &SuggestionRecord,
    tool: &str,
    failure: ErrorData,
) -> ErrorData {
    match write_suggestion(db, live_record) {
        Ok((key, _value)) => {
            let key_text = String::from_utf8_lossy(&key).into_owned();
            tracing::warn!(
                code = "SUGGESTION_RESOLUTION_ROLLED_BACK",
                tool,
                suggestion_id = %live_record.suggestion_id,
                routine_id = %live_record.routine_id,
                source = ?live_record.source,
                persisted_row_key = %key_text,
                failure = %failure.message,
                "the durable status flip was rolled back to Live after the decision failed to complete; the offer is retryable and was never left terminal without its grounding"
            );
            ErrorData::new(
                failure.code,
                format!(
                    "{}; the {tool} status flip on CF_KV {key_text} was ROLLED BACK: suggestion {} is status=live with resolved_ts_ns=null and resolution_note=null again (read-your-write verified), so this call left no durable trace and can be retried",
                    failure.message, live_record.suggestion_id
                ),
                failure.data,
            )
        }
        Err(rollback_error) => mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "SUGGESTION_RESOLUTION_ROLLBACK_FAILED for {}: {tool} could not complete the decision AND could not restore the live row. Original failure: {}. Rollback failure: {}. The row may be terminal without its grounding; the next call on this id takes the anchor-repair lane ({SUGGESTION_ANCHOR_REPAIR_CODE}), which completes the missing grounding write from the persisted row",
                live_record.suggestion_id, failure.message, rollback_error.message
            ),
        ),
    }
}

// === suggestion_decline: exact, grounded, idempotent dismissal (#2046) ===
//
// The generic `routine feedback` operation cannot serve as the dismissal
// surface: it names a routine, not the durable `suggestion/v1` row, so it can
// neither resolve WHICH offer was dismissed nor make replay falsifiable.
//
// Idempotency model (the standard idempotent-consumer contract):
//   * the idempotency key is the `suggestion_id` — minted by the producer when
//     the offer was created, content-stable, and NEVER regenerated by this
//     consumer from arrival time;
//   * the durable dedupe store is the `CF_KV suggestion/v1` row itself:
//     `status == Declined` IS the record that the effect already ran, and it is
//     written in the same flushed batch as the effect;
//   * a second dismissal of the same id performs zero side effects and rebuilds
//     the identical response by READING the persisted row and rescanning the
//     existing anchor — proof, not assertion;
//   * the same id carrying a DIFFERENT note is a conflict, not a retry, and is
//     refused with a structured mismatch naming both notes.

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionDeclineParams {
    /// The exact durable `suggestion/v1` id being dismissed.
    pub suggestion_id: String,
    /// Optional operator reason, folded verbatim into the persisted resolution
    /// note. Must be identical on a replay of the same id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Resolve as of this instant (replay/verification). Defaults to now, and is
    /// ignored entirely on a replay — the first decision's stamp is the truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ts_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestionDeclineResponse {
    pub source_of_truth: String,
    /// The row as it exists in `CF_KV` after the call — read back, not echoed.
    pub suggestion: SuggestionRecord,
    /// True when this call was a duplicate dismissal that changed nothing.
    pub replay: bool,
    pub declined_ts_ns: u64,
    pub resolution_note: String,
    pub persisted_row_key: String,
    pub persisted_row_sha256: String,
    /// True only when this call wrote new #856 feedback into
    /// `CF_ROUTINE_STATE`. A replay records none — one dismissal must not
    /// escalate the cooldown twice — and neither does a synthetic-namespace
    /// suggestion, which has no `CF_ROUTINE_STATE` row at all: read
    /// [`SuggestionDeclineResponse::feedback_store`] with this flag, never this
    /// flag alone.
    pub feedback_recorded: bool,
    /// Which durable store holds this suggestion's decline signal, named
    /// explicitly so `feedback_recorded=false` is never ambiguous. See
    /// [`FeedbackStore`].
    pub feedback_store: String,
    pub anchor: SuggestionOutcomeAnchorReport,
}

pub fn required_permissions_decline(_params: &SuggestionDeclineParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

fn validate_decline_note(note: Option<&str>) -> Result<Option<String>, ErrorData> {
    let Some(note) = note else {
        return Ok(None);
    };
    let trimmed = note.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > 512 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "suggestion_decline note must be at most 512 Unicode scalar values".to_owned(),
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "suggestion_decline note must not contain control characters".to_owned(),
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

/// Deterministic from the params alone, so a genuine retry of the same request
/// reconstructs the exact note already persisted.
fn decline_resolution_note(note: Option<&str>) -> String {
    match note {
        Some(note) => format!("declined by suggestion_decline: {note}"),
        None => "declined by suggestion_decline".to_owned(),
    }
}

/// Dismisses one exact suggestion, grounds the dismissal as a Calyx anchor, and
/// is a no-op on replay.
pub fn decline_suggestion(
    db: &Arc<Db>,
    params: &SuggestionDeclineParams,
) -> Result<SuggestionDeclineResponse, ErrorData> {
    validate_suggestion_id("suggestion_decline", &params.suggestion_id)?;
    let note = validate_decline_note(params.note.as_deref())?;
    let expected_note = decline_resolution_note(note.as_deref());

    let Some(existing) = load_suggestion_by_id(db, &params.suggestion_id)? else {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "SUGGESTION_NOT_FOUND: suggestion_id {} is not in CF_KV; suggestion_decline requires the exact durable suggestion/v1 id, not a routine_id",
                params.suggestion_id
            ),
        ));
    };

    // --- Replay lane: the effect already ran; prove it from storage. ---
    if existing.status == SuggestionStatus::Declined {
        let persisted_note = existing.resolution_note.clone().unwrap_or_default();
        if note.is_some() && persisted_note != expected_note {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "SUGGESTION_DECLINE_REPLAY_NOTE_MISMATCH: suggestion_id {} was already declined with resolution_note {persisted_note:?}; this call supplies {expected_note:?}. The same suggestion_id with different content is a conflict, not a retry — replay the original note or inspect the persisted row",
                    params.suggestion_id
                ),
            ));
        }
        let key = suggestion_key(&existing.routine_id, existing.created_ts_ns);
        let Some(value) = load_exact_kv_value(db, &key, "suggestion_decline replay readback")?
        else {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "SUGGESTION_DECLINE_REPLAY_ROW_MISSING: suggestion_id {} indexes to CF_KV key {} which holds no row",
                    params.suggestion_id,
                    String::from_utf8_lossy(&key)
                ),
            ));
        };
        let persisted: SuggestionRecord = decode_json(&value).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "SUGGESTION_DECLINE_REPLAY_DECODE_FAILED for {}: {error}",
                    params.suggestion_id
                ),
            )
        })?;
        if persisted != existing {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "SUGGESTION_DECLINE_REPLAY_ROW_DRIFTED: the indexed load and the direct point read of {} disagree",
                    params.suggestion_id
                ),
            ));
        }
        let declined_ts_ns = persisted.resolved_ts_ns.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "SUGGESTION_DECLINE_REPLAY_UNSTAMPED: suggestion {} is declined but carries no resolved_ts_ns",
                    params.suggestion_id
                ),
            )
        })?;
        let anchor = read_suggestion_outcome_anchor(db, &key, &value, &persisted)?;
        tracing::info!(
            code = "SUGGESTION_DECLINE_REPLAYED",
            suggestion_id = %params.suggestion_id,
            routine_id = %persisted.routine_id,
            declined_ts_ns,
            cx_id = %anchor.cx_id,
            "duplicate suggestion_decline was a no-op; response rebuilt from the persisted row and the physical Anchors CF"
        );
        return Ok(SuggestionDeclineResponse {
            source_of_truth: format!(
                "CF_KV suggestion/v1 row (point read) + {SUGGESTION_ANCHOR_SOURCE_OF_TRUTH}"
            ),
            declined_ts_ns,
            resolution_note: persisted_note,
            persisted_row_key: String::from_utf8_lossy(&key).into_owned(),
            persisted_row_sha256: sha256_hex(&value),
            replay: true,
            feedback_recorded: false,
            feedback_store: feedback_store_for(&persisted.routine_id).label().to_owned(),
            anchor,
            suggestion: persisted,
        });
    }

    // --- Mismatch lane: a resolved-but-not-declined row is not dismissable. ---
    if existing.status != SuggestionStatus::Live {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "SUGGESTION_DECLINE_STATUS_CONFLICT: suggestion_id {} has status {:?} (resolved_ts_ns={:?}, resolution_note={:?}); only a live suggestion can be declined, and only an already-declined one is an idempotent replay",
                params.suggestion_id,
                existing.status,
                existing.resolved_ts_ns,
                existing.resolution_note
            ),
        ));
    }

    // --- Effect lane: first dismissal of a live offer. ---
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "suggestion_decline refused under disk pressure: pressure_level={:?}",
                db.pressure_level()
            ),
        ));
    }
    let now = params.now_ts_ns.unwrap_or_else(now_ts_ns);
    let live_record = existing.clone();
    let mut record = existing;
    record.status = SuggestionStatus::Declined;
    record.resolved_ts_ns = Some(now);
    record.resolution_note = Some(expected_note.clone());
    let (key, value) = write_suggestion(db, &record)?;

    // Everything after the flip is compensated: the anchor must be written
    // against the value that is now in `CF_KV`, so the flip cannot go last, and
    // the previous code left the row `Declined` with no grounding whenever a
    // later step failed — a state no transition could leave (#2068 defect 3).
    // Now any failure here rolls the row back to `Live`, proven by read-back.
    //
    // #856 feedback: an EXPLICIT dismissal is a distinct, strong negative
    // signal — never conflated with the soft `ignored_timeout`/`abandoned`
    // outcomes the tick already records — so it escalates the decline cooldown.
    // Which store holds that cooldown depends on the id namespace, and for a
    // synthetic id it is this very row (see `record_terminal_feedback`).
    let completed = anchor_suggestion_outcome(db, &key, &value, &record).and_then(|anchor| {
        let feedback_recorded = record_terminal_feedback(
            db,
            &record.routine_id,
            RoutineFeedbackOutcome::Declined,
            now,
            &expected_note,
        )?;
        Ok((anchor, feedback_recorded))
    });
    let (anchor, feedback_recorded) = match completed {
        Ok(completed) => completed,
        Err(error) => {
            return Err(roll_back_suggestion_resolution(
                db,
                &live_record,
                "suggestion_decline",
                error,
            ));
        }
    };

    tracing::info!(
        code = "SUGGESTION_DECLINED",
        suggestion_id = %record.suggestion_id,
        routine_id = %record.routine_id,
        source = ?record.source,
        declined_ts_ns = now,
        cx_id = %anchor.cx_id,
        ledger_seq = anchor.ledger_seq.unwrap_or_default(),
        evidence_anchors = anchor.evidence_anchors.len(),
        feedback_recorded,
        "suggestion dismissed by exact id, grounded as a Calyx anchor, and its decline cooldown recorded"
    );

    Ok(SuggestionDeclineResponse {
        source_of_truth: format!(
            "CF_KV suggestion/v1 row (read-your-write) + {SUGGESTION_ANCHOR_SOURCE_OF_TRUTH}"
        ),
        declined_ts_ns: now,
        resolution_note: expected_note,
        persisted_row_key: String::from_utf8_lossy(&key).into_owned(),
        persisted_row_sha256: sha256_hex(&value),
        replay: false,
        feedback_recorded,
        feedback_store: feedback_store_for(&record.routine_id).label().to_owned(),
        anchor,
        suggestion: record,
    })
}

/// #856 feedback for an executed acceptance. Namespace-dispatched exactly like
/// every other terminal outcome, so accepting an `assist1-`/`next1-` suggestion
/// no longer dies against the `CF_ROUTINE_STATE` encoder AFTER the row has been
/// flipped and anchored.
pub fn record_suggestion_execution_feedback(
    db: &Arc<Db>,
    routine_id: &str,
    outcome: RoutineFeedbackOutcome,
    now_ns: u64,
    note: &str,
) -> Result<(), ErrorData> {
    record_terminal_feedback(db, routine_id, outcome, now_ns, note).map(|_| ())
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

/// Which durable store owns the #856 feedback and decline cooldown for a
/// suggestion, decided by its `routine_id` namespace (#2068 defect 1).
///
/// `CF_ROUTINE_STATE` is keyed by the mined-routine encoder — `rt1-` + 16
/// lowercase hex, enforced in `synapse-storage::routines::routine_state_key` —
/// so it physically cannot hold state for the synthetic per-offer ids this
/// module mints for sources that have no mined routine behind them
/// (`assist1-…` for an assist opportunity, `next1-…` for a kernel/graph-composed
/// next action). Routing those through the routine-state machinery is what made
/// every composed next action abort the whole tick and every synthetic decline
/// tear its row.
///
/// The answer is NOT to drop the cooldown: #856's decline cooldown is what stops
/// a dismissed offer being re-proposed on the next tick. It is to use the store
/// that actually owns these ids — the durable `CF_KV suggestion/v1` rows, which
/// this module already treats as its only source of truth ("a daemon restart
/// re-derives every cap and dedup decision from the persisted rows"). A
/// synthetic offer's terminal row IS its feedback event: `Declined`/`Expired`
/// escalate, `Accepted` resets, `Abandoned` is provenance only — the same
/// mapping `record_routine_feedback` applies, over the same
/// `feedback_cooldown_secs` curve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FeedbackStore {
    /// Mined routines (`rt1-` + 16 hex): the `CF_ROUTINE_STATE` row.
    RoutineState,
    /// Synthetic per-offer ids (`assist1-`, `next1-`): the `CF_KV
    /// suggestion/v1` rows themselves.
    SuggestionRows,
}

impl FeedbackStore {
    /// Stable, greppable name reported to callers.
    const fn label(self) -> &'static str {
        match self {
            Self::RoutineState => "cf_routine_state",
            Self::SuggestionRows => "cf_kv_suggestion_rows",
        }
    }
}

/// Pure namespace dispatch: the store is a structural fact of how the id was
/// minted, never configuration.
fn feedback_store_for(routine_id: &str) -> FeedbackStore {
    if routine_id.starts_with(ASSIST_ROUTINE_PREFIX)
        || routine_id.starts_with(NEXT_ACTION_ROUTINE_PREFIX)
    {
        FeedbackStore::SuggestionRows
    } else {
        // Anything else is claimed to be a mined routine id. It is NOT assumed
        // valid: the `CF_ROUTINE_STATE` encoder still rejects a malformed id,
        // loudly, which is the correct outcome for an id that is neither.
        FeedbackStore::RoutineState
    }
}

/// Records one terminal suggestion outcome as #856 feedback, in the store that
/// owns this id namespace. Returns whether a `CF_ROUTINE_STATE` row was written.
///
/// For a synthetic id nothing is written here and that is not a swallowed
/// failure: the caller has already persisted (or is about to persist) the
/// terminal `CF_KV` row that IS the feedback record for that namespace, and
/// `synthetic_feedback_suppressed` reads exactly those rows to compute the same
/// escalating cooldown. The skip is announced with its own daemon record so it
/// can never be mistaken for a missing signal.
fn record_terminal_feedback(
    db: &Arc<Db>,
    routine_id: &str,
    outcome: RoutineFeedbackOutcome,
    now_ns: u64,
    note: &str,
) -> Result<bool, ErrorData> {
    if feedback_store_for(routine_id) == FeedbackStore::SuggestionRows {
        tracing::info!(
            code = "SUGGESTION_FEEDBACK_RECORDED_ON_SUGGESTION_ROW",
            routine_id,
            outcome = ?outcome,
            now_ns,
            note,
            feedback_store = FeedbackStore::SuggestionRows.label(),
            "synthetic suggestion id has no CF_ROUTINE_STATE row; the terminal CF_KV suggestion/v1 row is this outcome's durable feedback record and drives its decline cooldown"
        );
        return Ok(false);
    }
    let params = RoutineFeedbackParams {
        routine_id: routine_id.to_owned(),
        outcome,
        note: Some(note.to_owned()),
        now_ts_ns: Some(now_ns),
    };
    record_routine_feedback(db, &params, SUGGESTION_ACTOR).map(|_| true)
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
    let (next_action_report, next_actions) = next_action_candidates(db, now, params)?;

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
                // Namespace-dispatched: a mined routine escalates its
                // `CF_ROUTINE_STATE` cooldown, a synthetic offer's cooldown is
                // this very `Expired` row. The old `RoutineIntent`-only guard
                // was the correct FIX for the crash but recorded the skip
                // nowhere; `record_terminal_feedback` now names the store it
                // used in the daemon record.
                record_terminal_feedback(
                    db,
                    &record.routine_id,
                    RoutineFeedbackOutcome::IgnoredTimeout,
                    now,
                    "suggestion expired (timeout)",
                )?;
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
        let suppressed = candidate_suppressed(db, &suggestions, &candidate.routine_id, now)?;
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
        // Synthetic `assist1-` id: its decline cooldown lives in the durable
        // suggestion rows, not `CF_ROUTINE_STATE`. Before #2068 this lane simply
        // passed `false` — a dismissed assist offer had no cooldown at all
        // beyond the per-routine window — because the only suppression helper
        // available would have crashed on the id.
        let suppressed = synthetic_feedback_suppressed(&suggestions, &candidate.routine_id, now);
        let outcome = gate_decision(
            &candidate.routine_id,
            candidate.confidence,
            RoutineLifecycle::Confirmed,
            suppressed,
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

    for candidate in &next_actions {
        // Synthetic `next1-` id: same store as the assist lane. Passing this to
        // `is_routine_suppressed` is what made every composed candidate abort
        // the whole tick with `ROUTINE_KEY_INVALID` (#2068 defect 1).
        let suppressed = synthetic_feedback_suppressed(&suggestions, &candidate.routine_id, now);
        let outcome = gate_decision(
            &candidate.routine_id,
            candidate.confidence,
            RoutineLifecycle::Confirmed,
            suppressed,
            now,
            now_minute,
            &live,
            &config,
        );
        let mut created_id = None;
        if outcome == GateOutcome::Surface && !params.dry_run {
            let record = build_next_action_suggestion(candidate, now, &config);
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
            source: SuggestionSource::KernelNextAction,
            confidence: candidate.confidence,
            outcome,
            suggestion_id: created_id,
            source_event_id: Some(candidate.grounding.target_cx_id.clone()),
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
        next_action: next_action_report,
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
        next_action: None,
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
        next_action: None,
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

/// #856 suppression for a MINED routine, from its `CF_ROUTINE_STATE` row.
///
/// Refuses a synthetic id up front instead of letting it reach the
/// `CF_ROUTINE_STATE` encoder: that encoder's `ROUTINE_KEY_INVALID` was raised
/// as a storage-write failure and aborted the entire `suggestion_tick`,
/// including the lanes that had nothing to do with the offending candidate
/// (#2068 defect 1). A synthetic id arriving here is a routing bug in this
/// module, so it fails loud as an internal error rather than being coerced.
fn is_routine_suppressed(db: &Arc<Db>, routine_id: &str, now: u64) -> Result<bool, ErrorData> {
    if feedback_store_for(routine_id) != FeedbackStore::RoutineState {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "SUGGESTION_SUPPRESSION_NAMESPACE_MISMATCH: routine_id {routine_id} is a synthetic per-offer id whose feedback lives in {}, not CF_ROUTINE_STATE; its cooldown must be read with synthetic_feedback_suppressed",
                FeedbackStore::SuggestionRows.label()
            ),
        ));
    }
    Ok(match load_state_row(db, routine_id)? {
        Some(state) => feedback_suppressed(&state, now),
        None => false,
    })
}

/// #856 decline cooldown for a synthetic per-offer id, derived from the durable
/// `CF_KV suggestion/v1` rows that ARE its feedback record.
///
/// Mirrors `record_routine_feedback` exactly, over the same
/// `feedback_cooldown_secs` curve: walking the routine's terminal rows newest
/// first, `Accepted` ends the streak (recovery), `Declined` and `Expired` (the
/// row-level form of `ignored_timeout`) each extend it, `Abandoned` is
/// provenance and neither extends nor resets. The cooldown then runs from the
/// most recent escalating outcome. `Live` rows are not outcomes and are skipped.
///
/// This is a derivation, not a cache: it reads the same persisted rows the
/// engine already loaded for its caps, so a daemon restart re-derives the
/// identical decision — the property the module header promises.
fn synthetic_feedback_suppressed(
    suggestions: &[(Vec<u8>, SuggestionRecord)],
    routine_id: &str,
    now: u64,
) -> bool {
    let mut outcomes: Vec<(u64, SuggestionStatus)> = suggestions
        .iter()
        .map(|(_key, record)| record)
        .filter(|record| record.routine_id == routine_id)
        .filter_map(|record| {
            let resolved_ts_ns = record.resolved_ts_ns?;
            match record.status {
                SuggestionStatus::Accepted
                | SuggestionStatus::Declined
                | SuggestionStatus::Expired
                | SuggestionStatus::Abandoned => Some((resolved_ts_ns, record.status)),
                SuggestionStatus::Live => None,
            }
        })
        .collect();
    // Newest first. Ties are broken by a fixed rank so the walk is total and
    // order-independent, and so a same-instant `Accepted` cannot truncate a
    // streak it did not follow: escalating outcomes are visited first.
    const fn tie_rank(status: SuggestionStatus) -> u8 {
        match status {
            SuggestionStatus::Declined | SuggestionStatus::Expired => 0,
            SuggestionStatus::Abandoned => 1,
            SuggestionStatus::Accepted | SuggestionStatus::Live => 2,
        }
    }
    outcomes.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| tie_rank(left.1).cmp(&tie_rank(right.1)))
    });

    let mut consecutive: u32 = 0;
    let mut streak_started_ts_ns = None;
    for (resolved_ts_ns, status) in outcomes {
        match status {
            SuggestionStatus::Accepted => break,
            SuggestionStatus::Declined | SuggestionStatus::Expired => {
                consecutive = consecutive.saturating_add(1);
                if streak_started_ts_ns.is_none() {
                    streak_started_ts_ns = Some(resolved_ts_ns);
                }
            }
            SuggestionStatus::Abandoned | SuggestionStatus::Live => {}
        }
    }
    let Some(latest_ts_ns) = streak_started_ts_ns else {
        return false;
    };
    let cooldown_ns = feedback_cooldown_secs(consecutive).saturating_mul(1_000_000_000);
    now < latest_ts_ns.saturating_add(cooldown_ns)
}

/// Suppression for one candidate, routed by the store that owns its id space.
fn candidate_suppressed(
    db: &Arc<Db>,
    suggestions: &[(Vec<u8>, SuggestionRecord)],
    routine_id: &str,
    now: u64,
) -> Result<bool, ErrorData> {
    match feedback_store_for(routine_id) {
        FeedbackStore::RoutineState => is_routine_suppressed(db, routine_id, now),
        FeedbackStore::SuggestionRows => {
            Ok(synthetic_feedback_suppressed(suggestions, routine_id, now))
        }
    }
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
    /// The grounded Calyx anchor written for the acceptance (#2046). `None`
    /// only for `dry_run`, which mutates nothing and therefore grounds nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_anchor: Option<SuggestionOutcomeAnchorReport>,
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

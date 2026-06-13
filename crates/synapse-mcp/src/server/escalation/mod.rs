//! Active attention & AFK escalation engine (#948, fleet-control epic #891).
//!
//! The Command Center promise is "an agent tells you when it needs you." The
//! attention surfaces shipped so far (title badge, peek panel, tray glance) all
//! assume the operator is at the PC looking at the screen. For unattended
//! overnight fleet runs the operator is away from the machine entirely, so the
//! escalation engine here ships **two tiers**:
//!
//! - **Tier 0 — on-PC, always on, no config:** a WinRT toast via the verified
//!   delivery path in [`super::notify_tools`]. Reaches the operator when they
//!   are at the machine but looking elsewhere.
//! - **Tier 1 — off-machine, opt-in, operator-supplied egress:** on a severity
//!   threshold, POST a structured packet to the operator's own webhook(s)
//!   (self-hosted ntfy, Pushover, a Telegram/Discord webhook, a phone-call
//!   service, …). Synapse ships **no** commercial push SaaS and requires none —
//!   identical philosophy to the operator-supplied local-model endpoints. With
//!   no egress configured the engine makes **zero** outbound network calls.
//!
//! Truth lives in `CF_KV`, never in daemon memory (the durable approval-queue
//! pattern, #867):
//! - `escalation/v1/config` — the operator policy (webhooks, threshold, quiet
//!   hours, ack window). A single row; absent row ⇒ Tier-0-only defaults.
//! - `escalation/v1/item/{escalation_id}` — current escalation state.
//! - `escalation/v1/audit/{escalation_id}/{at_unix_ms:020}-{event_id}` —
//!   append-only ladder log (opened, tier0 toast fired, each tier1 channel
//!   attempt with ok/failed+reason, acked, resolved, expired).
//!
//! The **trigger** is the attention-state transition at the `record_agent_events`
//! choke point (#898) — [`note_transition`] is called from
//! `agent_state::emit_transitions` after the authoritative `state_changed` rows
//! commit, so it fires for live transitions only and never for journal replay
//! on restart. Acknowledgment from any surface stops the ladder; an agent that
//! leaves its attention state auto-resolves the escalation.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{Local, Timelike};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{SCHEMA_VERSION, error_codes};
use synapse_storage::{Db, cf, decode_json, encode_json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::agent_state::{AgentLifecycleState, StateTransition};
use super::notify_tools::{NotifyHumanParams, NotifyKind, run_internal_toast, toast_tag_for};
use super::session_registry::unix_time_ms_now;
use super::{ErrorData, Json, Parameters, SynapseService, mcp_error, tool, tool_router};

#[cfg(test)]
mod tests;

const CONFIG_KEY: &str = "escalation/v1/config";
const ITEM_PREFIX: &str = "escalation/v1/item/";
const AUDIT_PREFIX: &str = "escalation/v1/audit/";
const ESCALATION_ID_PREFIX: &str = "esc1-";

/// Defaults chosen from DND/alert-fatigue research: a five-minute no-ack window
/// for ordinary escalations, one minute for critical (fastest escalation).
const DEFAULT_ACK_WINDOW_MS: u64 = 5 * 60 * 1_000;
const DEFAULT_CRITICAL_ACK_WINDOW_MS: u64 = 60 * 1_000;
/// TTL defaults: 7 days ordinary, 24h for sensitive (critical) escalations.
const DEFAULT_TTL_ORDINARY_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const DEFAULT_TTL_SENSITIVE_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_SCAN_ROWS: usize = 20_000;
const MAX_WEBHOOKS: usize = 8;
const MAX_URL_CHARS: usize = 2_048;
const MAX_NAME_CHARS: usize = 64;
const MAX_SECRET_CHARS: usize = 512;
const WEBHOOK_TIMEOUT_MS: u64 = 15_000;
const WORKER_TICK_MS: u64 = 1_000;

// ---------------------------------------------------------------------------
// Severity & attention-state mapping (the response ladder, decided up front)
// ---------------------------------------------------------------------------

/// Escalation severity. Ordered: a higher severity escalates faster, ignores
/// quiet hours (critical only), and uses the verified-delivery error toast.
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    /// Done / ready for review — digest-class; toast only, never an off-machine
    /// interrupt.
    Low,
    /// Needs input / awaiting approval — toast + sound; push and escalate on
    /// no-ack.
    Medium,
    /// Stuck / irreversible error — toast + sound + flash; push immediately,
    /// fastest escalation, routes even during quiet hours.
    Critical,
}

impl Severity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::Critical => "critical",
        }
    }

    /// On-PC toast severity. Stuck/critical maps to the error kind (long
    /// duration), needs-input to warning, ready-for-review to info.
    const fn notify_kind(self) -> NotifyKind {
        match self {
            Self::Low => NotifyKind::Info,
            Self::Medium => NotifyKind::Warning,
            Self::Critical => NotifyKind::Error,
        }
    }
}

/// Maps an attention state to its escalation severity, or `None` when the state
/// is not attention-worthy (working/idle/spawning/dead → no escalation).
fn severity_for(state: AgentLifecycleState) -> Option<Severity> {
    match state {
        AgentLifecycleState::ReadyForReview => Some(Severity::Low),
        AgentLifecycleState::NeedsInput | AgentLifecycleState::AwaitingApproval => {
            Some(Severity::Medium)
        }
        AgentLifecycleState::Stuck => Some(Severity::Critical),
        AgentLifecycleState::Spawning
        | AgentLifecycleState::Working
        | AgentLifecycleState::Idle
        | AgentLifecycleState::Dead => None,
    }
}

// ---------------------------------------------------------------------------
// Operator policy
// ---------------------------------------------------------------------------

/// A single operator-supplied off-machine egress. Channels fire in list order:
/// the first on escalation open, each subsequent one only after the no-ack
/// window elapses with no acknowledgment.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebhookChannel {
    /// Operator label for the channel (shown in the ladder audit).
    pub name: String,
    /// Target URL the structured packet is POSTed to.
    pub url: String,
    /// Optional shared secret. When set, the request carries an
    /// `X-Synapse-Signature: sha256=<hex>` HMAC over the exact JSON body so the
    /// operator's listener can authenticate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// Quiet-hours window in **local** wall-clock minutes since midnight. Wraps
/// midnight when `start_minute > end_minute`. Suppresses low/medium off-machine
/// pushes; critical still routes (coverage-safe — never silently disables
/// critical coverage).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuietHours {
    pub start_minute: u16,
    pub end_minute: u16,
}

impl QuietHours {
    fn contains(self, minute_of_day: u16) -> bool {
        match self.start_minute.cmp(&self.end_minute) {
            // Degenerate window: treat as "no quiet hours" rather than "always".
            std::cmp::Ordering::Equal => false,
            std::cmp::Ordering::Less => {
                minute_of_day >= self.start_minute && minute_of_day < self.end_minute
            }
            // Wraps midnight.
            std::cmp::Ordering::Greater => {
                minute_of_day >= self.start_minute || minute_of_day < self.end_minute
            }
        }
    }
}

/// Operator escalation policy. The absent-row default is Tier-0-only: no
/// webhooks, so no outbound network calls are ever attempted.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EscalationPolicy {
    pub schema_version: u32,
    /// Ordered off-machine egress ladder. Empty ⇒ Tier 0 only.
    #[serde(default)]
    pub webhooks: Vec<WebhookChannel>,
    /// Minimum severity that triggers an off-machine push. Default `medium`:
    /// `low` (done/ready) stays a digest-class toast and never interrupts.
    pub min_tier1_severity: Severity,
    /// No-ack window before the next ladder channel fires (ordinary).
    pub ack_window_ms: u64,
    /// No-ack window for critical escalations (faster).
    pub critical_ack_window_ms: u64,
    pub ttl_ordinary_ms: u64,
    pub ttl_sensitive_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHours>,
    pub updated_at_unix_ms: u64,
}

impl Default for EscalationPolicy {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            webhooks: Vec::new(),
            min_tier1_severity: Severity::Medium,
            ack_window_ms: DEFAULT_ACK_WINDOW_MS,
            critical_ack_window_ms: DEFAULT_CRITICAL_ACK_WINDOW_MS,
            ttl_ordinary_ms: DEFAULT_TTL_ORDINARY_MS,
            ttl_sensitive_ms: DEFAULT_TTL_SENSITIVE_MS,
            quiet_hours: None,
            updated_at_unix_ms: 0,
        }
    }
}

impl EscalationPolicy {
    fn window_for(&self, severity: Severity) -> u64 {
        match severity {
            Severity::Critical => self.critical_ack_window_ms,
            Severity::Low | Severity::Medium => self.ack_window_ms,
        }
    }

    fn ttl_for(&self, severity: Severity) -> u64 {
        match severity {
            Severity::Critical => self.ttl_sensitive_ms,
            Severity::Low | Severity::Medium => self.ttl_ordinary_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Escalation item & ladder records
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EscalationStatus {
    /// Open and actively escalating.
    Pending,
    /// Acknowledged by a human/surface — ladder stopped, still open until the
    /// agent leaves the attention state.
    Acked,
    /// The agent left the attention state (resumed/finished) — auto-closed.
    Resolved,
    /// TTL elapsed with no acknowledgment.
    Expired,
}

impl EscalationStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acked => "acked",
            Self::Resolved => "resolved",
            Self::Expired => "expired",
        }
    }

    const fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Acked)
    }
}

/// The minimum context package carried by every escalation (issue requirement):
/// plain-language action/state, the agent's reason, a reversibility flag, the
/// session id for audit correlation, the approval-deadline timestamp, and a
/// deep link to the agent-detail page.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EscalationContext {
    pub action: String,
    pub reason: String,
    pub reversible: bool,
    #[serde(default)]
    pub alternatives: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    pub agent_detail_deep_link: String,
    pub approval_deadline_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub evidence: Value,
}

/// One off-machine delivery attempt — recorded ok or failed; never summarized
/// away (alert fatigue / silent-drop avoidance).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChannelAttempt {
    pub channel_name: String,
    pub url_host: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub signed: bool,
    pub at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EscalationItem {
    pub schema_version: u32,
    pub escalation_id: String,
    /// Attribution anchor: spawn id for spawned agents, otherwise the session id.
    pub anchor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub severity: Severity,
    /// The attention state that opened this escalation (snake_case).
    pub attention_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    pub context: EscalationContext,
    pub status: EscalationStatus,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    /// True once the on-PC toast was delivered AND verified in Action Center.
    pub tier0_fired: bool,
    /// True when off-machine push is suppressed because the escalation opened
    /// inside a quiet-hours window (low/medium only; critical never suppressed).
    pub tier1_quiet_suppressed: bool,
    /// Whether this escalation is eligible for off-machine push at all
    /// (severity ≥ threshold, ≥1 webhook configured, not quiet-suppressed).
    pub tier1_eligible: bool,
    /// Count of off-machine channels already attempted.
    pub ladder_index: u32,
    /// When the next ladder channel may fire; `None` when no channel remains,
    /// it is not tier1-eligible, or the escalation is no longer pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_escalate_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub channel_attempts: Vec<ChannelAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Storage keys / encode-decode
// ---------------------------------------------------------------------------

fn item_key(escalation_id: &str) -> Vec<u8> {
    format!("{ITEM_PREFIX}{escalation_id}").into_bytes()
}

fn audit_key(escalation_id: &str, at_unix_ms: u64, event_id: &str) -> Vec<u8> {
    format!("{AUDIT_PREFIX}{escalation_id}/{at_unix_ms:020}-{event_id}").into_bytes()
}

fn storage_error(error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

fn encode_item(item: &EscalationItem) -> Result<Vec<u8>, ErrorData> {
    encode_json(item).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "escalation item encode failed for {}: {error}",
                item.escalation_id
            ),
        )
    })
}

/// Writes the escalation item plus one append-only audit row in a single
/// pressure-bypass batch, then reads both rows back to prove the write landed.
fn write_item_and_audit(
    db: &Db,
    item: &EscalationItem,
    event: &str,
    detail: Value,
) -> Result<(), ErrorData> {
    let item_key = item_key(&item.escalation_id);
    let item_value = encode_item(item)?;
    let event_id = Uuid::now_v7().simple().to_string();
    let at = item.updated_at_unix_ms;
    let audit = json!({
        "schema_version": SCHEMA_VERSION,
        "escalation_id": item.escalation_id,
        "event_id": event_id,
        "event": event,
        "at_unix_ms": at,
        "anchor": item.anchor,
        "severity": item.severity.as_str(),
        "status": item.status.as_str(),
        "ladder_index": item.ladder_index,
        "detail": detail,
    });
    let audit_key = audit_key(&item.escalation_id, at, &event_id);
    let audit_value = serde_json::to_vec(&audit).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("escalation audit encode failed for {}: {error}", item.escalation_id),
        )
    })?;
    db.put_batch_pressure_bypass(
        cf::CF_KV,
        [
            (item_key.clone(), item_value),
            (audit_key.clone(), audit_value),
        ],
    )
    .map_err(storage_error)?;
    // Physical readback: prove both rows are present (FSV-by-construction).
    read_exact_row(db, &item_key)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "escalation item row absent immediately after write for {}",
                item.escalation_id
            ),
        )
    })?;
    read_exact_row(db, &audit_key)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "escalation audit row absent immediately after write for {}",
                item.escalation_id
            ),
        )
    })?;
    Ok(())
}

fn read_exact_row(db: &Db, key: &[u8]) -> Result<Option<Vec<u8>>, ErrorData> {
    let rows = db.scan_cf_prefix(cf::CF_KV, key).map_err(storage_error)?;
    Ok(rows.into_iter().find(|(k, _)| k == key).map(|(_, v)| v))
}

fn read_item(db: &Db, escalation_id: &str) -> Result<Option<EscalationItem>, ErrorData> {
    let key = item_key(escalation_id);
    match read_exact_row(db, &key)? {
        Some(value) => Ok(Some(decode_item(escalation_id, &value)?)),
        None => Ok(None),
    }
}

fn decode_item(escalation_id: &str, value: &[u8]) -> Result<EscalationItem, ErrorData> {
    decode_json::<EscalationItem>(value).map_err(|error| {
        mcp_error(
            error.code(),
            format!("escalation item decode failed for {escalation_id}: {error}"),
        )
    })
}

/// All escalation items, newest scan order. Bounded by [`MAX_SCAN_ROWS`].
fn scan_items(db: &Db) -> Result<Vec<EscalationItem>, ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, ITEM_PREFIX.as_bytes())
        .map_err(storage_error)?;
    if rows.len() > MAX_SCAN_ROWS {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "escalation item scan exceeded {MAX_SCAN_ROWS} rows ({}); refusing partial result",
                rows.len()
            ),
        ));
    }
    let mut items = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let id = String::from_utf8_lossy(&key);
        let id = id.strip_prefix(ITEM_PREFIX).unwrap_or(&id);
        items.push(decode_item(id, &value)?);
    }
    Ok(items)
}

fn open_items_for_anchor(db: &Db, anchor: &str) -> Result<Vec<EscalationItem>, ErrorData> {
    Ok(scan_items(db)?
        .into_iter()
        .filter(|item| item.anchor == anchor && item.status.is_open())
        .collect())
}

// ---------------------------------------------------------------------------
// Policy storage
// ---------------------------------------------------------------------------

fn load_policy(db: &Db) -> Result<EscalationPolicy, ErrorData> {
    match read_exact_row(db, CONFIG_KEY.as_bytes())? {
        Some(value) => decode_json::<EscalationPolicy>(&value).map_err(|error| {
            mcp_error(
                error.code(),
                format!("escalation policy decode failed: {error}"),
            )
        }),
        None => Ok(EscalationPolicy::default()),
    }
}

fn store_policy(db: &Db, policy: &EscalationPolicy) -> Result<(), ErrorData> {
    let value = encode_json(policy).map_err(|error| {
        mcp_error(error.code(), format!("escalation policy encode failed: {error}"))
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(CONFIG_KEY.as_bytes().to_vec(), value)])
        .map_err(storage_error)?;
    read_exact_row(db, CONFIG_KEY.as_bytes())?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "escalation policy row absent immediately after write",
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Quiet hours
// ---------------------------------------------------------------------------

fn current_local_minute_of_day() -> u16 {
    let now = Local::now();
    u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(0)
}

fn quiet_now(policy: &EscalationPolicy, minute_of_day: u16) -> bool {
    policy
        .quiet_hours
        .map(|quiet| quiet.contains(minute_of_day))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Engine: transition hook (sync), called from agent_state::emit_transitions
// ---------------------------------------------------------------------------

/// Process-wide wake signal for the async escalation worker. Installed by
/// [`spawn_worker`]; absent in unit tests that drive [`process_pending`]
/// directly, in which case [`note_transition`] simply skips the wake.
static WORKER_SIGNAL: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

fn wake_worker() {
    if let Some(signal) = WORKER_SIGNAL.get() {
        signal.notify_one();
    }
}

/// Hook called once per live state transition after the authoritative
/// `state_changed` rows committed. Opens or resolves durable escalations. A
/// storage failure here is logged loudly but never unwinds the caller — the
/// primary journal rows already committed and the attention state is
/// re-derivable from them.
pub(crate) fn note_transition(db: &Db, transition: &StateTransition, now_unix_ms: u64) {
    if let Err(error) = note_transition_inner(db, transition, now_unix_ms) {
        tracing::error!(
            code = "ESCALATION_TRANSITION_FAILED",
            anchor = %transition.anchor,
            state_to = transition.state_to.as_str(),
            detail = %error.message,
            "escalation engine could not record a state transition; attention escalation may be missed for this edge"
        );
    }
}

fn note_transition_inner(
    db: &Db,
    transition: &StateTransition,
    now_unix_ms: u64,
) -> Result<(), ErrorData> {
    let new_state = transition.state_to.as_str();
    // 1. Auto-resolve any open escalation for this anchor whose attention state
    //    differs from the new state. Leaving the attention state (resume/finish)
    //    or transitioning to a different attention state supersedes the old one.
    let mut superseded = false;
    for mut item in open_items_for_anchor(db, &transition.anchor)? {
        if item.attention_state == new_state {
            continue;
        }
        item.status = EscalationStatus::Resolved;
        item.updated_at_unix_ms = now_unix_ms;
        item.next_escalate_at_unix_ms = None;
        item.closed_reason = Some(format!("state_change:{new_state}"));
        write_item_and_audit(
            db,
            &item,
            "resolved",
            json!({ "reason": "state_change", "new_state": new_state }),
        )?;
        superseded = true;
        tracing::info!(
            code = "ESCALATION_RESOLVED",
            escalation_id = %item.escalation_id,
            anchor = %item.anchor,
            new_state,
            "readback=CF_KV escalation resolved by state change"
        );
    }

    // 2. Open a new escalation when the new state is attention-worthy and no
    //    open escalation already exists for this exact (anchor, state).
    let Some(severity) = severity_for(transition.state_to) else {
        if superseded {
            wake_worker();
        }
        return Ok(());
    };
    let already_open = open_items_for_anchor(db, &transition.anchor)?
        .into_iter()
        .any(|item| item.attention_state == new_state);
    if already_open {
        return Ok(());
    }
    let policy = load_policy(db)?;
    open_escalation(db, transition, severity, &policy, now_unix_ms)?;
    wake_worker();
    Ok(())
}

fn open_escalation(
    db: &Db,
    transition: &StateTransition,
    severity: Severity,
    policy: &EscalationPolicy,
    now_unix_ms: u64,
) -> Result<EscalationItem, ErrorData> {
    let escalation_id = format!("{ESCALATION_ID_PREFIX}{}", Uuid::now_v7().simple());
    let in_quiet = quiet_now(policy, current_local_minute_of_day());
    // Critical routes even during quiet hours; low/medium are suppressed.
    let quiet_suppressed = in_quiet && severity < Severity::Critical;
    let tier1_eligible =
        !policy.webhooks.is_empty() && severity >= policy.min_tier1_severity && !quiet_suppressed;
    let ttl = policy.ttl_for(severity);
    let context = build_context(transition, severity, now_unix_ms, ttl);
    let item = EscalationItem {
        schema_version: SCHEMA_VERSION,
        escalation_id,
        anchor: transition.anchor.clone(),
        spawn_id: transition.spawn_id.clone(),
        session_id: transition.session_id.clone(),
        severity,
        attention_state: transition.state_to.as_str().to_owned(),
        reason_code: Some(transition.reason_code.clone()),
        context,
        status: EscalationStatus::Pending,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl),
        tier0_fired: false,
        tier1_quiet_suppressed: quiet_suppressed,
        tier1_eligible,
        ladder_index: 0,
        // First off-machine channel may fire immediately when eligible.
        next_escalate_at_unix_ms: tier1_eligible.then_some(now_unix_ms),
        channel_attempts: Vec::new(),
        acked_at_unix_ms: None,
        acked_via: None,
        closed_reason: None,
    };
    write_item_and_audit(
        db,
        &item,
        "opened",
        json!({
            "severity": severity.as_str(),
            "tier1_eligible": tier1_eligible,
            "quiet_suppressed": quiet_suppressed,
            "configured_webhooks": policy.webhooks.len(),
        }),
    )?;
    tracing::info!(
        code = "ESCALATION_OPENED",
        escalation_id = %item.escalation_id,
        anchor = %item.anchor,
        severity = severity.as_str(),
        attention_state = %item.attention_state,
        tier1_eligible,
        quiet_suppressed,
        "readback=CF_KV escalation opened"
    );
    Ok(item)
}

fn build_context(
    transition: &StateTransition,
    severity: Severity,
    now_unix_ms: u64,
    ttl: u64,
) -> EscalationContext {
    let action = match transition.state_to {
        AgentLifecycleState::NeedsInput => "Agent needs your input to continue",
        AgentLifecycleState::AwaitingApproval => "Agent is waiting for your approval",
        AgentLifecycleState::ReadyForReview => "Agent finished and is ready for review",
        AgentLifecycleState::Stuck => "Agent appears stuck and needs attention",
        _ => "Agent needs attention",
    }
    .to_owned();
    // Stuck/critical escalations flag potential irreversibility so the operator
    // treats them with care; ordinary attention states are reversible.
    let reversible = severity != Severity::Critical;
    let alternatives = transition
        .evidence
        .get("alternatives")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    EscalationContext {
        action,
        reason: transition.reason_code.clone(),
        reversible,
        alternatives,
        waiting_for: transition.waiting_for.clone(),
        agent_detail_deep_link: format!("/agents/{}", transition.anchor),
        approval_deadline_unix_ms: now_unix_ms.saturating_add(ttl),
        evidence: transition.evidence.clone(),
    }
}

// ---------------------------------------------------------------------------
// Acknowledgment
// ---------------------------------------------------------------------------

/// Acknowledges an escalation from any surface, stopping the off-machine ladder
/// while leaving the escalation open until the agent leaves the attention
/// state. Idempotent: acking an already-acked/closed escalation reports the
/// existing state without re-firing.
fn ack_escalation(
    db: &Db,
    escalation_id: &str,
    via: &str,
    note: Option<&str>,
    now_unix_ms: u64,
) -> Result<EscalationItem, ErrorData> {
    let mut item = read_item(db, escalation_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("escalation {escalation_id} not found"),
        )
    })?;
    if item.status != EscalationStatus::Pending {
        // Already acked/resolved/expired — honest idempotent report, no re-fire.
        return Ok(item);
    }
    item.status = EscalationStatus::Acked;
    item.updated_at_unix_ms = now_unix_ms;
    item.acked_at_unix_ms = Some(now_unix_ms);
    item.acked_via = Some(via.to_owned());
    item.next_escalate_at_unix_ms = None;
    write_item_and_audit(
        db,
        &item,
        "acked",
        json!({ "via": via, "note": note }),
    )?;
    tracing::info!(
        code = "ESCALATION_ACKED",
        escalation_id = %item.escalation_id,
        via,
        "readback=CF_KV escalation acknowledged; ladder stopped"
    );
    Ok(item)
}

// ---------------------------------------------------------------------------
// Worker: Tier 0 toast + Tier 1 webhook ladder + TTL expiry (async)
// ---------------------------------------------------------------------------

/// Outcome of one [`process_pending`] sweep, for worker logs and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessReport {
    pub tier0_fired: usize,
    pub tier1_fired: usize,
    pub tier1_failed: usize,
    pub expired: usize,
    pub scanned: usize,
}

/// One escalation-delivery sweep. Fires the on-PC toast for any pending
/// escalation not yet delivered, fires the next due off-machine channel, and
/// expires escalations past their TTL. Idempotent and re-entrant-safe: only
/// acts on durable row state, so a missed worker tick is recovered on the next.
pub(crate) async fn process_pending(db: &Arc<Db>, now_unix_ms: u64) -> Result<ProcessReport, ErrorData> {
    let mut report = ProcessReport::default();
    let items = scan_items(db)?;
    report.scanned = items.len();
    for mut item in items {
        if !item.status.is_open() {
            continue;
        }
        // TTL expiry takes precedence over further delivery.
        if now_unix_ms >= item.expires_at_unix_ms {
            item.status = EscalationStatus::Expired;
            item.updated_at_unix_ms = now_unix_ms;
            item.next_escalate_at_unix_ms = None;
            item.closed_reason = Some("ttl_expired".to_owned());
            write_item_and_audit(db, &item, "expired", json!({ "ttl_ms": item.expires_at_unix_ms }))?;
            report.expired += 1;
            tracing::warn!(
                code = "ESCALATION_EXPIRED",
                escalation_id = %item.escalation_id,
                "readback=CF_KV escalation expired with no acknowledgment"
            );
            continue;
        }

        let mut dirty = false;

        // Tier 0 — on-PC toast (always, regardless of egress config).
        if !item.tier0_fired {
            match fire_tier0(&item).await {
                Ok(()) => {
                    item.tier0_fired = true;
                    item.updated_at_unix_ms = now_unix_ms;
                    write_item_and_audit(db, &item, "tier0_toast_fired", json!({}))?;
                    report.tier0_fired += 1;
                    dirty = false; // already persisted
                }
                Err(error) => {
                    tracing::error!(
                        code = "ESCALATION_TIER0_FAILED",
                        escalation_id = %item.escalation_id,
                        detail = %error.message,
                        "on-PC toast delivery failed; will retry next sweep"
                    );
                    // Leave tier0_fired=false so the next sweep retries.
                }
            }
        }

        // Tier 1 — off-machine push ladder (only when the row is still pending;
        // an acked escalation has next_escalate_at cleared).
        if item.status == EscalationStatus::Pending
            && item.tier1_eligible
            && let Some(due_at) = item.next_escalate_at_unix_ms
            && now_unix_ms >= due_at
        {
            let policy = load_policy(db)?;
            let index = item.ladder_index as usize;
            if let Some(channel) = policy.webhooks.get(index).cloned() {
                let attempt = deliver_webhook(&channel, &item, now_unix_ms).await;
                if attempt.ok {
                    report.tier1_fired += 1;
                } else {
                    report.tier1_failed += 1;
                }
                let event_detail = json!({
                    "channel_name": attempt.channel_name,
                    "ok": attempt.ok,
                    "http_status": attempt.http_status,
                    "error": attempt.error,
                });
                item.channel_attempts.push(attempt);
                item.ladder_index += 1;
                item.updated_at_unix_ms = now_unix_ms;
                // Schedule the next ladder channel only if one remains.
                item.next_escalate_at_unix_ms = if (item.ladder_index as usize) < policy.webhooks.len() {
                    Some(now_unix_ms.saturating_add(policy.window_for(item.severity)))
                } else {
                    None
                };
                write_item_and_audit(db, &item, "tier1_channel_attempt", event_detail)?;
                dirty = false;
            } else {
                // No channel at this index (config shrank): stop the ladder.
                item.next_escalate_at_unix_ms = None;
                dirty = true;
            }
        }

        if dirty {
            item.updated_at_unix_ms = now_unix_ms;
            write_item_and_audit(db, &item, "updated", json!({}))?;
        }
    }
    Ok(report)
}

async fn fire_tier0(item: &EscalationItem) -> Result<(), ErrorData> {
    let title = format!(
        "Synapse: {} [{}]",
        item.context.action,
        item.severity.as_str()
    );
    let mut body = item.context.reason.clone();
    if let Some(waiting) = &item.context.waiting_for {
        body = format!("{body}\nWaiting on: {waiting}");
    }
    body = format!("{body}\nAgent: {}", item.anchor);
    let params = NotifyHumanParams {
        title,
        body,
        kind: item.severity.notify_kind(),
        // Dedupe on the escalation id so repeated sweeps before dismissal do not
        // stack duplicate toasts.
        dedupe_key: Some(format!("escalation:{}", item.escalation_id)),
        suppress_popup: false,
    };
    let tag = toast_tag_for(params.dedupe_key.as_deref());
    run_internal_toast(params, tag, Vec::new()).await.map(|_response| ())
}

async fn deliver_webhook(
    channel: &WebhookChannel,
    item: &EscalationItem,
    now_unix_ms: u64,
) -> ChannelAttempt {
    let url_host = reqwest::Url::parse(&channel.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unparseable>".to_owned());
    let mut attempt = ChannelAttempt {
        channel_name: channel.name.clone(),
        url_host,
        ok: false,
        http_status: None,
        error: None,
        signed: channel.secret.is_some(),
        at_unix_ms: now_unix_ms,
    };
    let payload = webhook_payload(channel, item);
    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            attempt.error = Some(format!("payload serialize failed: {error}"));
            return attempt;
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(WEBHOOK_TIMEOUT_MS))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            attempt.error = Some(format!("http client build failed: {error}"));
            return attempt;
        }
    };
    let mut request = client
        .post(&channel.url)
        .header("Content-Type", "application/json");
    if let Some(secret) = &channel.secret {
        let signature = hmac_sha256_hex(secret.as_bytes(), &body);
        request = request.header("X-Synapse-Signature", format!("sha256={signature}"));
    }
    match request.body(body).send().await {
        Ok(response) => {
            let status = response.status();
            attempt.http_status = Some(status.as_u16());
            if status.is_success() {
                attempt.ok = true;
            } else {
                attempt.error = Some(format!("non-2xx response: {status}"));
            }
        }
        Err(error) => {
            attempt.error = Some(format!("request failed: {error}"));
        }
    }
    attempt
}

fn webhook_payload(channel: &WebhookChannel, item: &EscalationItem) -> Value {
    json!({
        "schema": "synapse.escalation.v1",
        "channel": channel.name,
        "escalation_id": item.escalation_id,
        "severity": item.severity.as_str(),
        "attention_state": item.attention_state,
        "anchor": item.anchor,
        "spawn_id": item.spawn_id,
        "session_id": item.session_id,
        "reason_code": item.reason_code,
        "ladder_index": item.ladder_index,
        "created_at_unix_ms": item.created_at_unix_ms,
        "context": item.context,
    })
}

/// HMAC-SHA256 (RFC 2104) over `msg` with `key`, hex-encoded. Implemented over
/// the already-vendored `sha2` so no extra dependency is pulled in.
fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        block_key[..digest.len()].copy_from_slice(&digest);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        ipad[index] ^= block_key[index];
        opad[index] ^= block_key[index];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let digest = outer.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

// ---------------------------------------------------------------------------
// Worker spawn (wired in http/transport.rs)
// ---------------------------------------------------------------------------

/// Spawns the escalation delivery worker. Installs the process-wide wake signal
/// so [`note_transition`] can prompt an immediate sweep, then loops on that
/// signal plus a steady tick (for the no-ack ladder and TTL) until shutdown.
pub(crate) fn spawn_worker(db: Arc<Db>, shutdown: CancellationToken) -> JoinHandle<()> {
    let signal = Arc::new(tokio::sync::Notify::new());
    let _already = WORKER_SIGNAL.set(Arc::clone(&signal));
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(WORKER_TICK_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::info!(
            code = "ESCALATION_WORKER_STARTED",
            tick_ms = WORKER_TICK_MS,
            "escalation delivery worker running"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!(code = "ESCALATION_WORKER_STOPPED", "stopping escalation worker");
                    break;
                }
                _ = signal.notified() => {}
                _ = interval.tick() => {}
            }
            match process_pending(&db, unix_time_ms_now()).await {
                Ok(report) if report.tier0_fired + report.tier1_fired + report.tier1_failed + report.expired > 0 => {
                    tracing::info!(
                        code = "ESCALATION_SWEEP",
                        tier0_fired = report.tier0_fired,
                        tier1_fired = report.tier1_fired,
                        tier1_failed = report.tier1_failed,
                        expired = report.expired,
                        scanned = report.scanned,
                        "escalation sweep delivered"
                    );
                }
                Ok(_quiet) => {}
                Err(error) => {
                    tracing::error!(
                        code = "ESCALATION_SWEEP_FAILED",
                        detail = %error.message,
                        "escalation sweep failed; will retry next tick"
                    );
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// MCP tools
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationConfigSetParams {
    /// Ordered off-machine egress ladder. Replaces the existing list. Empty
    /// list ⇒ Tier 0 only (no outbound calls). Omit to leave webhooks unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhooks: Option<Vec<WebhookChannel>>,
    /// Minimum severity to push off-machine: "low" | "medium" | "critical".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tier1_severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_ack_window_ms: Option<u64>,
    /// Quiet-hours window in local minutes since midnight. Pass `null` to clear.
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    /// When true, applies `quiet_hours` (including clearing it when null).
    #[serde(default)]
    pub set_quiet_hours: bool,
}

/// `escalation_config_get` takes no inputs; an explicit empty struct keeps the
/// generated input schema a closed object (the tool-surface contract).
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationConfigGetParams {}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookChannelView {
    pub name: String,
    pub url: String,
    /// Secrets are never echoed; this only reports whether one is configured.
    pub secret_configured: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationConfigResponse {
    pub webhooks: Vec<WebhookChannelView>,
    pub min_tier1_severity: Severity,
    pub ack_window_ms: u64,
    pub critical_ack_window_ms: u64,
    pub ttl_ordinary_ms: u64,
    pub ttl_sensitive_ms: u64,
    pub quiet_hours: Option<QuietHours>,
    pub updated_at_unix_ms: u64,
    /// True when no egress is configured — the Tier-0-only default that makes
    /// zero outbound network calls.
    pub tier0_only: bool,
}

impl EscalationConfigResponse {
    fn from_policy(policy: &EscalationPolicy) -> Self {
        Self {
            webhooks: policy
                .webhooks
                .iter()
                .map(|channel| WebhookChannelView {
                    name: channel.name.clone(),
                    url: channel.url.clone(),
                    secret_configured: channel.secret.is_some(),
                })
                .collect(),
            min_tier1_severity: policy.min_tier1_severity,
            ack_window_ms: policy.ack_window_ms,
            critical_ack_window_ms: policy.critical_ack_window_ms,
            ttl_ordinary_ms: policy.ttl_ordinary_ms,
            ttl_sensitive_ms: policy.ttl_sensitive_ms,
            quiet_hours: policy.quiet_hours,
            updated_at_unix_ms: policy.updated_at_unix_ms,
            tier0_only: policy.webhooks.is_empty(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationListParams {
    /// Filter by status: "pending" | "acked" | "resolved" | "expired".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<EscalationStatus>,
    /// Filter by attribution anchor (spawn id or session id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// Max rows to return (default 50, max 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationListResponse {
    pub escalations: Vec<EscalationItem>,
    pub total_open: usize,
    pub returned: usize,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationAckParams {
    pub escalation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationAckResponse {
    pub escalation: EscalationItem,
    /// True when this call performed the ack; false when already acked/closed.
    pub newly_acked: bool,
}

fn validate_config(params: &EscalationConfigSetParams) -> Result<(), ErrorData> {
    if let Some(webhooks) = &params.webhooks {
        if webhooks.len() > MAX_WEBHOOKS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("at most {MAX_WEBHOOKS} webhooks are allowed; got {}", webhooks.len()),
            ));
        }
        for channel in webhooks {
            if channel.name.trim().is_empty() || channel.name.chars().count() > MAX_NAME_CHARS {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("webhook name must be 1..={MAX_NAME_CHARS} chars"),
                ));
            }
            if channel.url.chars().count() > MAX_URL_CHARS {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("webhook url must be <= {MAX_URL_CHARS} chars"),
                ));
            }
            let url = reqwest::Url::parse(&channel.url).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("webhook url for '{}' is not a valid URL: {error}", channel.name),
                )
            })?;
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("webhook url for '{}' must be http:// or https://", channel.name),
                ));
            }
            if let Some(secret) = &channel.secret
                && secret.chars().count() > MAX_SECRET_CHARS
            {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("webhook secret for '{}' must be <= {MAX_SECRET_CHARS} chars", channel.name),
                ));
            }
        }
    }
    if let Some(window) = params.ack_window_ms
        && window == 0
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "ack_window_ms must be >= 1",
        ));
    }
    if let Some(window) = params.critical_ack_window_ms
        && window == 0
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "critical_ack_window_ms must be >= 1",
        ));
    }
    if let Some(quiet) = params.quiet_hours
        && (quiet.start_minute >= 1440 || quiet.end_minute >= 1440)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "quiet_hours minutes must be in 0..1440",
        ));
    }
    Ok(())
}

#[tool_router(router = escalation_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Configure the AFK escalation engine: operator-supplied off-machine webhook egress (Tier 1), the severity threshold for pushing off-machine, the no-ack ladder windows, and quiet hours. Synapse ships no push SaaS — you bring your own transport (self-hosted ntfy, Pushover, a Telegram/Discord webhook, a phone-call service). With no webhooks configured the engine is Tier-0-only (on-PC toast) and makes zero outbound network calls. Secrets are stored but never echoed back."
    )]
    pub async fn escalation_config_set(
        &self,
        params: Parameters<EscalationConfigSetParams>,
    ) -> Result<Json<EscalationConfigResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "escalation_config_set",
            "tool.invocation kind=escalation_config_set"
        );
        validate_config(&params)?;
        let db = self.m3_storage()?;
        let mut policy = load_policy(&db)?;
        if let Some(webhooks) = params.webhooks {
            policy.webhooks = webhooks;
        }
        if let Some(severity) = params.min_tier1_severity {
            policy.min_tier1_severity = severity;
        }
        if let Some(window) = params.ack_window_ms {
            policy.ack_window_ms = window;
        }
        if let Some(window) = params.critical_ack_window_ms {
            policy.critical_ack_window_ms = window;
        }
        if params.set_quiet_hours {
            policy.quiet_hours = params.quiet_hours;
        }
        policy.updated_at_unix_ms = unix_time_ms_now();
        store_policy(&db, &policy)?;
        Ok(Json(EscalationConfigResponse::from_policy(&policy)))
    }

    #[tool(
        description = "Read the current AFK escalation policy: configured off-machine webhooks (secrets redacted to a boolean), severity threshold, no-ack ladder windows, TTLs, and quiet hours. Reports tier0_only=true when no egress is configured."
    )]
    pub async fn escalation_config_get(
        &self,
        _params: Parameters<EscalationConfigGetParams>,
    ) -> Result<Json<EscalationConfigResponse>, ErrorData> {
        let db = self.m3_storage()?;
        let policy = load_policy(&db)?;
        Ok(Json(EscalationConfigResponse::from_policy(&policy)))
    }

    #[tool(
        description = "List durable attention escalations (CF_KV) with full ladder state: severity, attention state, the minimum context package, on-PC toast delivery, each off-machine channel attempt (ok/failed+reason — never summarized away), acknowledgment, and TTL. Filter by status and anchor. Read-only; this is the data the dashboard hygiene/attention panels render."
    )]
    pub async fn escalation_list(
        &self,
        params: Parameters<EscalationListParams>,
    ) -> Result<Json<EscalationListResponse>, ErrorData> {
        let params = params.0;
        let db = self.m3_storage()?;
        let mut items = scan_items(&db)?;
        items.sort_by_key(|item| std::cmp::Reverse(item.created_at_unix_ms));
        let total_open = items.iter().filter(|item| item.status.is_open()).count();
        let limit = params.limit.unwrap_or(50).min(500) as usize;
        let filtered: Vec<EscalationItem> = items
            .into_iter()
            .filter(|item| params.status.map(|s| s == item.status).unwrap_or(true))
            .filter(|item| params.anchor.as_deref().map(|a| a == item.anchor).unwrap_or(true))
            .take(limit)
            .collect();
        Ok(Json(EscalationListResponse {
            returned: filtered.len(),
            total_open,
            escalations: filtered,
        }))
    }

    #[tool(
        description = "Acknowledge an escalation, immediately stopping its off-machine no-ack ladder. The escalation stays open (acked) until the agent leaves its attention state, which auto-resolves it. Idempotent: acking an already-acked/closed escalation reports the existing state without re-firing. The ack is physically written to the escalation audit log."
    )]
    pub async fn escalation_ack(
        &self,
        params: Parameters<EscalationAckParams>,
    ) -> Result<Json<EscalationAckResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "escalation_ack",
            escalation_id = %params.escalation_id,
            "tool.invocation kind=escalation_ack"
        );
        let db = self.m3_storage()?;
        let was_pending = read_item(&db, &params.escalation_id)?
            .map(|item| item.status == EscalationStatus::Pending)
            .unwrap_or(false);
        let escalation = ack_escalation(
            &db,
            &params.escalation_id,
            "escalation_ack_tool",
            params.note.as_deref(),
            unix_time_ms_now(),
        )?;
        Ok(Json(EscalationAckResponse {
            newly_acked: was_pending && escalation.status == EscalationStatus::Acked,
            escalation,
        }))
    }
}


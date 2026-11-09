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

use super::agent_state::{AgentLifecycleState, AgentStateRead, StateTransition};
use super::notify_tools::{
    NotifyHumanParams, NotifyKind, ToastCleanupReport, ToastRemovalOutcome, remove_internal_toast,
    remove_orphaned_escalation_toasts, run_internal_toast, toast_tag_for,
};
use super::session_registry::unix_time_ms_now;
use super::{ErrorData, Json, Parameters, SynapseService, mcp_error, tool, tool_router};
use crate::m3::approvals::{
    ApprovalAllow, ApprovalAuditRecord, ApprovalItemRecord, ApprovalKind, ApprovalStatus,
    ApprovalTimeoutDecision, ApprovalToastState,
};

type CfKvRow = (Vec<u8>, Vec<u8>);
type CfKvRows = Vec<CfKvRow>;

const CONFIG_KEY: &str = "escalation/v1/config";
const ITEM_PREFIX: &str = "escalation/v1/item/";
const AUDIT_PREFIX: &str = "escalation/v1/audit/";
const ORPHAN_TOAST_AUDIT_PREFIX: &str = "escalation/v1/toast_orphan_cleanup/";
const ESCALATION_ID_PREFIX: &str = "esc1-";
const APPROVAL_ITEM_PREFIX: &str = "approval/v1/item/";
const APPROVAL_AUDIT_PREFIX: &str = "approval/v1/audit/";

/// Defaults chosen from DND/alert-fatigue research: a five-minute no-ack window
/// for ordinary escalations, one minute for critical (fastest escalation).
const DEFAULT_ACK_WINDOW_MS: u64 = 5 * 60 * 1_000;
const DEFAULT_CRITICAL_ACK_WINDOW_MS: u64 = 60 * 1_000;
/// TTL defaults: 7 days ordinary, 24h for sensitive (critical) escalations.
const DEFAULT_TTL_ORDINARY_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const DEFAULT_TTL_SENSITIVE_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_SCAN_ROWS: usize = 20_000;
const SCAN_CHUNK_ROWS: usize = 4_096;
const TERMINAL_ITEM_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const TERMINAL_ITEM_RETAIN_ROWS: usize = 5_000;
const DELETE_BATCH_ROWS: usize = 512;
const MAX_WEBHOOKS: usize = 8;
const MAX_URL_CHARS: usize = 2_048;
const MAX_NAME_CHARS: usize = 64;
const MAX_SECRET_CHARS: usize = 512;
const WEBHOOK_TIMEOUT_MS: u64 = 15_000;
const WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL: u32 = 3;
const WEBHOOK_RETRY_BASE_BACKOFF_MS: u64 = 30_000;
const WEBHOOK_RETRY_MAX_BACKOFF_MS: u64 = DEFAULT_ACK_WINDOW_MS;
const WORKER_TICK_MS: u64 = 1_000;
const AMBIENT_SILENT_TIMEOUT_SUPPRESSED: &str = "ambient_unprobeable_silent_timeout";

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
    #[serde(default)]
    pub ladder_index: u32,
    #[serde(default)]
    pub attempt_number: u32,
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
    /// Durable approvals-inbox row that lets any approval surface ack this
    /// escalation and stop its ladder.
    pub approval_id: String,
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
    /// Last removal readback for the Tier 0 Action Center toast once this
    /// escalation no longer needs to interrupt the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier0_toast_removed: Option<ToastRemovalOutcome>,
    /// True when Tier 0 should avoid a popup. Quiet-hours rows still write a
    /// digest-only Action Center entry; policy-suppressed rows skip Tier 0 and
    /// record `tier0_suppressed_reason`.
    pub tier0_quiet_digest: bool,
    /// Why Tier 0 is not fired at all. Distinct from quiet digest, which still
    /// writes an Action Center row with popup suppressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier0_suppressed_reason: Option<String>,
    /// True when off-machine push is suppressed because the escalation opened
    /// inside a quiet-hours window (low/medium only; critical never suppressed).
    pub tier1_quiet_suppressed: bool,
    /// Why off-machine push is suppressed for non-quiet-hours policy reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier1_suppressed_reason: Option<String>,
    /// Why no linked pending approvals-inbox row was created for this
    /// escalation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_suppressed_reason: Option<String>,
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

fn orphan_toast_audit_key(at_unix_ms: u64, event_id: &str) -> Vec<u8> {
    format!("{ORPHAN_TOAST_AUDIT_PREFIX}{at_unix_ms:020}-{event_id}").into_bytes()
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
    write_item_and_audit_with_extra_rows(db, item, event, detail, Vec::new())
}

fn write_item_and_audit_with_extra_rows(
    db: &Db,
    item: &EscalationItem,
    event: &str,
    detail: Value,
    extra_rows: CfKvRows,
) -> Result<(), ErrorData> {
    let extra_keys = extra_rows
        .iter()
        .map(|(key, _value)| key.clone())
        .collect::<Vec<_>>();
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
            format!(
                "escalation audit encode failed for {}: {error}",
                item.escalation_id
            ),
        )
    })?;
    let mut rows = vec![
        (item_key.clone(), item_value),
        (audit_key.clone(), audit_value),
    ];
    rows.extend(extra_rows);
    db.put_batch_pressure_bypass(cf::CF_KV, rows)
        .map_err(storage_error)?;
    // Physical write-readback guard: prove both rows are present immediately.
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
    for key in extra_keys {
        read_exact_row(db, &key)?.ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "linked approval row absent immediately after write for key {}",
                    String::from_utf8_lossy(&key)
                ),
            )
        })?;
    }
    Ok(())
}

fn approval_item_key(approval_id: &str) -> Vec<u8> {
    format!("{APPROVAL_ITEM_PREFIX}{approval_id}").into_bytes()
}

fn approval_audit_key(approval_id: &str, at_unix_ms: u64, event_id: &str) -> Vec<u8> {
    format!("{APPROVAL_AUDIT_PREFIX}{approval_id}/{at_unix_ms:020}-{event_id}").into_bytes()
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

#[derive(Clone, Debug)]
struct EscalationItemRow {
    key: Vec<u8>,
    item: EscalationItem,
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

fn scan_item_rows(db: &Db) -> Result<Vec<EscalationItemRow>, ErrorData> {
    let mut start = ITEM_PREFIX.as_bytes().to_vec();
    let mut out = Vec::new();
    loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_KV, &start, SCAN_CHUNK_ROWS)
            .map_err(storage_error)?;
        if rows.is_empty() {
            break;
        }
        let mut stop = false;
        let mut last_key = None;
        for (key, value) in rows {
            if !key.starts_with(ITEM_PREFIX.as_bytes()) {
                stop = true;
                break;
            }
            let id = String::from_utf8_lossy(&key);
            let id = id.strip_prefix(ITEM_PREFIX).unwrap_or(&id);
            out.push(EscalationItemRow {
                key: key.clone(),
                item: decode_item(id, &value)?,
            });
            last_key = Some(key);
        }
        if stop || !more {
            break;
        }
        let Some(key) = last_key else {
            break;
        };
        start = key_after(&key);
    }
    Ok(out)
}

fn delete_keys(db: &Db, mut keys: Vec<Vec<u8>>, context: &str) -> Result<usize, ErrorData> {
    keys.sort();
    keys.dedup();
    let deleted = keys.len();
    for chunk in keys.chunks(DELETE_BATCH_ROWS) {
        db.delete_batch(cf::CF_KV, chunk.iter().cloned())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("{context} failed to delete terminal rows: {error}"),
                )
            })?;
    }
    Ok(deleted)
}

fn prune_terminal_item_rows(
    db: &Db,
    now_unix_ms: u64,
    rows: &[EscalationItemRow],
) -> Result<usize, ErrorData> {
    let mut terminal = rows
        .iter()
        .filter(|row| !row.item.status.is_open())
        .map(|row| (row.item.updated_at_unix_ms, row.key.clone()))
        .collect::<Vec<_>>();
    terminal.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut delete = terminal
        .iter()
        .filter(|(updated_at, _key)| {
            updated_at.saturating_add(TERMINAL_ITEM_RETENTION_MS) <= now_unix_ms
        })
        .map(|(_updated_at, key)| key.clone())
        .collect::<Vec<_>>();

    if terminal.len() > TERMINAL_ITEM_RETAIN_ROWS {
        let over_cap = terminal.len() - TERMINAL_ITEM_RETAIN_ROWS;
        delete.extend(
            terminal
                .iter()
                .take(over_cap)
                .map(|(_updated_at, key)| key.clone()),
        );
    }

    let deleted = delete_keys(db, delete, "escalation terminal retention")?;
    if deleted > 0 {
        tracing::info!(
            code = "ESCALATION_ITEM_RETENTION_PRUNED",
            scanned_rows = rows.len(),
            terminal_rows = terminal.len(),
            deleted_rows = deleted,
            retain_terminal_rows = TERMINAL_ITEM_RETAIN_ROWS,
            retention_ms = TERMINAL_ITEM_RETENTION_MS,
            "readback=CF_KV escalation terminal item rows pruned"
        );
    } else if rows.len() > MAX_SCAN_ROWS {
        tracing::warn!(
            code = "ESCALATION_ITEM_QUEUE_LARGE",
            scanned_rows = rows.len(),
            terminal_rows = terminal.len(),
            retain_terminal_rows = TERMINAL_ITEM_RETAIN_ROWS,
            "escalation item scan exceeded historical hard limit but continued"
        );
    }
    Ok(deleted)
}

fn prune_terminal_items(db: &Db, now_unix_ms: u64) -> Result<usize, ErrorData> {
    let rows = scan_item_rows(db)?;
    prune_terminal_item_rows(db, now_unix_ms, &rows)
}

/// All escalation items. Large queues are scanned in bounded RocksDB windows;
/// terminal item rows are compacted by the sweep/list paths instead of making
/// the queue fail closed at the historical row limit.
fn scan_items(db: &Db) -> Result<Vec<EscalationItem>, ErrorData> {
    scan_item_rows(db).map(|rows| rows.into_iter().map(|row| row.item).collect())
}

fn open_items_for_anchor(db: &Db, anchor: &str) -> Result<Vec<EscalationItem>, ErrorData> {
    Ok(scan_items(db)?
        .into_iter()
        .filter(|item| item.anchor == anchor && item.status.is_open())
        .collect())
}

pub(crate) fn acked_open_attention_anchors(db: &Db) -> Result<Vec<String>, ErrorData> {
    let mut anchors: Vec<String> = scan_items(db)?
        .into_iter()
        .filter(|item| item.status == EscalationStatus::Acked)
        .map(|item| item.anchor)
        .collect();
    anchors.sort();
    anchors.dedup();
    Ok(anchors)
}

fn open_tier0_toast_tags(db: &Db) -> Result<Vec<String>, ErrorData> {
    Ok(scan_items(db)?
        .into_iter()
        .filter(|item| {
            item.status.is_open() && item.tier0_fired && item.tier0_toast_removed.is_none()
        })
        .map(|item| escalation_toast_tag(&item.escalation_id))
        .collect())
}

fn write_orphan_toast_cleanup_audit(
    db: &Db,
    report: &ToastCleanupReport,
    now_unix_ms: u64,
) -> Result<Option<String>, ErrorData> {
    if report.candidates == 0 && report.failed == 0 && report.error_code.is_none() {
        return Ok(None);
    }
    let event_id = Uuid::now_v7().simple().to_string();
    let row_key = orphan_toast_audit_key(now_unix_ms, &event_id);
    let row = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "event": "orphan_tier0_toast_cleanup",
        "at_unix_ms": now_unix_ms,
        "report": report,
    });
    let value = encode_json(&row).map_err(|error| {
        mcp_error(
            error.code(),
            format!("orphan toast cleanup audit encode failed: {error}"),
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.clone(), value)])
        .map_err(storage_error)?;
    read_exact_row(db, &row_key)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "orphan toast cleanup audit row absent immediately after write for key {}",
                String::from_utf8_lossy(&row_key)
            ),
        )
    })?;
    Ok(Some(hex_encode_bytes(&row_key)))
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
        mcp_error(
            error.code(),
            format!("escalation policy encode failed: {error}"),
        )
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

fn approval_rows_for_opened_escalation(
    item: &EscalationItem,
    now_unix_ms: u64,
) -> Result<CfKvRows, ErrorData> {
    let payload = json!({
        "schema": "synapse.escalation.approval.v1",
        "escalation_id": item.escalation_id,
        "anchor": item.anchor,
        "spawn_id": item.spawn_id,
        "session_id": item.session_id,
        "severity": item.severity.as_str(),
        "attention_state": item.attention_state,
        "reason_code": item.reason_code,
        "context": item.context,
    });
    let payload_json = serde_json::to_string(&payload).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "approval payload encode failed for escalation {}: {error}",
                item.escalation_id
            ),
        )
    })?;
    let approval = ApprovalItemRecord {
        schema_version: SCHEMA_VERSION,
        approval_id: item.approval_id.clone(),
        kind: ApprovalKind::AgentEscalation,
        status: ApprovalStatus::Pending,
        title: format!(
            "Synapse escalation: {} [{}]",
            item.context.action,
            item.severity.as_str()
        ),
        body: format!("{}\nAgent: {}", item.context.reason, item.anchor),
        payload_json: Some(payload_json),
        dedupe_key: Some(format!("escalation:{}", item.escalation_id)),
        destructive: !item.context.reversible,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: Some(item.expires_at_unix_ms),
        timeout_decision: ApprovalTimeoutDecision::Ignored,
        requested_by_session: "agent_attention_escalation".to_owned(),
        decided_by_session: None,
        decided_at_unix_ms: None,
        decision_note: None,
        allow: ApprovalAllow::for_kind(ApprovalKind::AgentEscalation),
        edited_args_json: None,
        operator_response: None,
        toast: ApprovalToastState {
            requested: false,
            suppress_popup: item.tier0_quiet_digest,
            actionable_buttons: false,
            activation_id: None,
            protocol_handler_registered: None,
            unavailable_reason: None,
            notify_tag: None,
            notify_group: None,
            notification_setting: None,
            verified_in_history: None,
        },
    };
    let approval_event_id = Uuid::now_v7().simple().to_string();
    let approval_audit = ApprovalAuditRecord {
        schema_version: SCHEMA_VERSION,
        approval_id: item.approval_id.clone(),
        event_id: approval_event_id.clone(),
        event: "requested".to_owned(),
        at_unix_ms: now_unix_ms,
        by_session: "agent_attention_escalation".to_owned(),
        before_status: None,
        after_status: ApprovalStatus::Pending,
        note: Some(format!(
            "linked escalation {} opened for {}",
            item.escalation_id, item.attention_state
        )),
    };
    let approval_item_value = encode_json(&approval).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "approval item encode failed for linked escalation {}: {error}",
                item.escalation_id
            ),
        )
    })?;
    let approval_audit_value = encode_json(&approval_audit).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "approval audit encode failed for linked escalation {}: {error}",
                item.escalation_id
            ),
        )
    })?;
    Ok(vec![
        (approval_item_key(&item.approval_id), approval_item_value),
        (
            approval_audit_key(&item.approval_id, now_unix_ms, &approval_event_id),
            approval_audit_value,
        ),
    ])
}

fn approval_status_is_terminal(status: ApprovalStatus) -> bool {
    matches!(
        status,
        ApprovalStatus::Accepted | ApprovalStatus::Declined | ApprovalStatus::Ignored
    )
}

fn linked_approval_terminal_rows(
    db: &Db,
    item: &EscalationItem,
    event: &str,
    note: String,
) -> Result<CfKvRows, ErrorData> {
    let approval_key = approval_item_key(&item.approval_id);
    let Some(value) = read_exact_row(db, &approval_key)? else {
        return Ok(Vec::new());
    };
    let mut approval = decode_json::<ApprovalItemRecord>(&value).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "linked approval item decode failed for escalation {} approval {}: {error}",
                item.escalation_id, item.approval_id
            ),
        )
    })?;
    if approval.kind != ApprovalKind::AgentEscalation
        || approval_status_is_terminal(approval.status)
    {
        return Ok(Vec::new());
    }
    let before_status = approval.status;
    approval.status = ApprovalStatus::Ignored;
    approval.updated_at_unix_ms = item.updated_at_unix_ms;
    approval.expires_at_unix_ms = None;
    approval.decided_by_session = Some("agent_attention_escalation".to_owned());
    approval.decided_at_unix_ms = Some(item.updated_at_unix_ms);
    approval.decision_note = Some(note);

    let audit_event_id = Uuid::now_v7().simple().to_string();
    let audit = ApprovalAuditRecord {
        schema_version: SCHEMA_VERSION,
        approval_id: approval.approval_id.clone(),
        event_id: audit_event_id.clone(),
        event: event.to_owned(),
        at_unix_ms: item.updated_at_unix_ms,
        by_session: "agent_attention_escalation".to_owned(),
        before_status: Some(before_status),
        after_status: ApprovalStatus::Ignored,
        note: approval.decision_note.clone(),
    };
    let approval_value = encode_json(&approval).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "linked approval item encode failed for escalation {} approval {}: {error}",
                item.escalation_id, item.approval_id
            ),
        )
    })?;
    let audit_key = approval_audit_key(
        &approval.approval_id,
        item.updated_at_unix_ms,
        &audit_event_id,
    );
    let audit_value = encode_json(&audit).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "linked approval audit encode failed for escalation {} approval {}: {error}",
                item.escalation_id, item.approval_id
            ),
        )
    })?;
    Ok(vec![
        (approval_key, approval_value),
        (audit_key, audit_value),
    ])
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
        let approval_rows = linked_approval_terminal_rows(
            db,
            &item,
            "linked_escalation_resolved",
            format!(
                "linked escalation {} resolved by state_change:{new_state}",
                item.escalation_id
            ),
        )?;
        write_item_and_audit_with_extra_rows(
            db,
            &item,
            "resolved",
            json!({ "reason": "state_change", "new_state": new_state }),
            approval_rows,
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
    let approval_id = format!("apr1-{}", Uuid::now_v7().simple());
    let in_quiet = quiet_now(policy, current_local_minute_of_day());
    // Critical routes even during quiet hours; low/medium are suppressed.
    let quiet_suppressed = in_quiet && severity < Severity::Critical;
    let policy_suppressed_reason = operator_interrupt_suppressed_reason(transition);
    let tier1_eligible = !policy.webhooks.is_empty()
        && severity >= policy.min_tier1_severity
        && !quiet_suppressed
        && policy_suppressed_reason.is_none();
    let ttl = policy.ttl_for(severity);
    let context = build_context(transition, severity, now_unix_ms, ttl);
    let status = if policy_suppressed_reason.is_some() {
        EscalationStatus::Acked
    } else {
        EscalationStatus::Pending
    };
    let item = EscalationItem {
        schema_version: SCHEMA_VERSION,
        escalation_id,
        approval_id,
        anchor: transition.anchor.clone(),
        spawn_id: transition.spawn_id.clone(),
        session_id: transition.session_id.clone(),
        severity,
        attention_state: transition.state_to.as_str().to_owned(),
        reason_code: Some(transition.reason_code.clone()),
        context,
        status,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl),
        tier0_fired: false,
        tier0_toast_removed: None,
        tier0_quiet_digest: quiet_suppressed || policy_suppressed_reason.is_some(),
        tier0_suppressed_reason: policy_suppressed_reason.clone(),
        tier1_quiet_suppressed: quiet_suppressed,
        tier1_suppressed_reason: policy_suppressed_reason.clone(),
        approval_suppressed_reason: policy_suppressed_reason.clone(),
        tier1_eligible,
        ladder_index: 0,
        // First off-machine channel may fire immediately when eligible.
        next_escalate_at_unix_ms: tier1_eligible.then_some(now_unix_ms),
        channel_attempts: Vec::new(),
        acked_at_unix_ms: policy_suppressed_reason.as_ref().map(|_reason| now_unix_ms),
        acked_via: policy_suppressed_reason
            .as_ref()
            .map(|reason| format!("policy:{reason}")),
        closed_reason: None,
    };
    let approval_rows = if item.approval_suppressed_reason.is_some() {
        Vec::new()
    } else {
        approval_rows_for_opened_escalation(&item, now_unix_ms)?
    };
    write_item_and_audit_with_extra_rows(
        db,
        &item,
        "opened",
        json!({
            "severity": severity.as_str(),
            "tier1_eligible": tier1_eligible,
            "quiet_suppressed": quiet_suppressed,
            "policy_suppressed_reason": policy_suppressed_reason,
            "configured_webhooks": policy.webhooks.len(),
            "approval_id": item.approval_id,
            "approval_row_written": !approval_rows.is_empty(),
        }),
        approval_rows,
    )?;
    tracing::info!(
        code = "ESCALATION_OPENED",
        escalation_id = %item.escalation_id,
        anchor = %item.anchor,
        severity = severity.as_str(),
        attention_state = %item.attention_state,
        tier1_eligible,
        quiet_suppressed,
        policy_suppressed_reason = item.tier0_suppressed_reason.as_deref(),
        "readback=CF_KV escalation opened"
    );
    Ok(item)
}

fn operator_interrupt_suppressed_reason(transition: &StateTransition) -> Option<String> {
    if transition.state_to == AgentLifecycleState::Stuck
        && transition.reason_code == "silent_timeout_unprobeable"
        && transition
            .spawn_id
            .as_deref()
            .is_some_and(|spawn_id| spawn_id.starts_with("agent-spawn-ambient-"))
        && transition
            .evidence
            .get("probed_pid")
            .is_some_and(Value::is_null)
    {
        Some(AMBIENT_SILENT_TIMEOUT_SUPPRESSED.to_owned())
    } else {
        None
    }
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
    write_item_and_audit(db, &item, "acked", json!({ "via": via, "note": note }))?;
    tracing::info!(
        code = "ESCALATION_ACKED",
        escalation_id = %item.escalation_id,
        via,
        "readback=CF_KV escalation acknowledged; ladder stopped"
    );
    Ok(item)
}

/// Bridges the durable approval queue (#867) back to the escalation ladder. Any
/// decision on an `agent_escalation` approval means a human surface saw it, so
/// the off-machine no-ack ladder stops immediately.
pub(crate) fn ack_from_approval_item_decision(
    db: &Db,
    approval: &ApprovalItemRecord,
    decision: &str,
    note: Option<&str>,
    by_session: &str,
    now_unix_ms: u64,
) -> Result<Option<EscalationItem>, ErrorData> {
    if approval.kind != ApprovalKind::AgentEscalation {
        return Ok(None);
    }
    let payload_json = approval.payload_json.as_deref().ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "agent escalation approval {} missing payload_json",
                approval.approval_id
            ),
        )
    })?;
    let payload = serde_json::from_str::<Value>(payload_json).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "agent escalation approval {} payload_json decode failed: {error}",
                approval.approval_id
            ),
        )
    })?;
    let escalation_id = payload
        .get("escalation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "agent escalation approval {} payload_json missing escalation_id",
                    approval.approval_id
                ),
            )
        })?;
    let via = format!("approval_decide:{decision}");
    let item = ack_escalation(db, escalation_id, &via, note, now_unix_ms)?;
    tracing::info!(
        code = "ESCALATION_ACKED_FROM_APPROVAL",
        escalation_id,
        approval_id = %approval.approval_id,
        decision,
        by_session,
        "approval decision acknowledged escalation and stopped the ladder"
    );
    Ok(Some(item))
}

// ---------------------------------------------------------------------------
// Worker: Tier 0 toast + Tier 1 webhook ladder + TTL expiry (async)
// ---------------------------------------------------------------------------

/// Outcome of one [`process_pending`] sweep, for worker logs and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessReport {
    pub tier0_fired: usize,
    pub tier0_removed: usize,
    pub tier0_remove_failed: usize,
    pub tier1_fired: usize,
    pub tier1_failed: usize,
    pub expired: usize,
    pub terminal_resolved: usize,
    pub linked_approvals_closed: usize,
    pub scanned: usize,
}

/// One escalation-delivery sweep. Fires the on-PC toast for any pending
/// escalation not yet delivered, fires the next due off-machine channel, and
/// expires escalations past their TTL. Idempotent and re-entrant-safe: only
/// acts on durable row state, so a missed worker tick is recovered on the next.
pub(crate) async fn process_pending(
    db: &Arc<Db>,
    now_unix_ms: u64,
) -> Result<ProcessReport, ErrorData> {
    let mut report = ProcessReport::default();
    let rows = scan_item_rows(db)?;
    let pruned = prune_terminal_item_rows(db, now_unix_ms, &rows)?;
    let items = if pruned > 0 {
        scan_items(db)?
    } else {
        rows.into_iter().map(|row| row.item).collect()
    };
    let terminal_agent_reads: Vec<AgentStateRead> = super::agent_state::reads(now_unix_ms)
        .into_iter()
        .filter(|read| read.state == AgentLifecycleState::Dead)
        .collect();
    report.scanned = items.len();
    for mut item in items {
        if !item.status.is_open() {
            if remove_tier0_if_terminal(db, &mut item).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            if matches!(
                item.status,
                EscalationStatus::Resolved | EscalationStatus::Expired
            ) {
                let approval_rows = linked_approval_terminal_rows(
                    db,
                    &item,
                    "linked_escalation_already_closed",
                    format!(
                        "linked escalation {} is already {}",
                        item.escalation_id,
                        item.status.as_str()
                    ),
                )?;
                if !approval_rows.is_empty() {
                    item.updated_at_unix_ms = now_unix_ms;
                    write_item_and_audit_with_extra_rows(
                        db,
                        &item,
                        "linked_approval_closed",
                        json!({
                            "reason": "linked_escalation_already_closed",
                            "status": item.status.as_str(),
                            "approval_id": &item.approval_id,
                        }),
                        approval_rows,
                    )?;
                    report.linked_approvals_closed += 1;
                }
            }
            continue;
        }
        if item.status == EscalationStatus::Acked {
            if remove_tier0_if_terminal(db, &mut item).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            continue;
        }
        if let Some(agent_state) = terminal_agent_read_for_item(&terminal_agent_reads, &item) {
            item.status = EscalationStatus::Resolved;
            item.updated_at_unix_ms = now_unix_ms;
            item.next_escalate_at_unix_ms = None;
            let reason = agent_state.reason_code.as_deref().unwrap_or("dead");
            item.closed_reason = Some(format!("terminal_agent_state:dead:{reason}"));
            let approval_rows = linked_approval_terminal_rows(
                db,
                &item,
                "linked_escalation_resolved",
                format!(
                    "linked escalation {} resolved because anchor is terminal dead:{reason}",
                    item.escalation_id
                ),
            )?;
            if !approval_rows.is_empty() {
                report.linked_approvals_closed += 1;
            }
            write_item_and_audit_with_extra_rows(
                db,
                &item,
                "resolved",
                json!({
                    "reason": "terminal_agent_state",
                    "agent_state": agent_state,
                }),
                approval_rows,
            )?;
            report.terminal_resolved += 1;
            tracing::info!(
                code = "ESCALATION_RESOLVED",
                escalation_id = %item.escalation_id,
                anchor = %item.anchor,
                reason,
                "readback=CF_KV escalation resolved because anchor is terminal"
            );
            if remove_tier0_if_terminal(db, &mut item).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            continue;
        }
        // TTL expiry takes precedence over further delivery.
        if now_unix_ms >= item.expires_at_unix_ms {
            item.status = EscalationStatus::Expired;
            item.updated_at_unix_ms = now_unix_ms;
            item.next_escalate_at_unix_ms = None;
            item.closed_reason = Some("ttl_expired".to_owned());
            let approval_rows = linked_approval_terminal_rows(
                db,
                &item,
                "linked_escalation_expired",
                format!("linked escalation {} expired by ttl", item.escalation_id),
            )?;
            if !approval_rows.is_empty() {
                report.linked_approvals_closed += 1;
            }
            write_item_and_audit_with_extra_rows(
                db,
                &item,
                "expired",
                json!({ "ttl_ms": item.expires_at_unix_ms }),
                approval_rows,
            )?;
            report.expired += 1;
            tracing::warn!(
                code = "ESCALATION_EXPIRED",
                escalation_id = %item.escalation_id,
                "readback=CF_KV escalation expired with no acknowledgment"
            );
            if remove_tier0_if_terminal(db, &mut item).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            continue;
        }

        let mut dirty = false;

        // Tier 0 — on-PC toast (always, regardless of egress config).
        if item.tier0_suppressed_reason.is_none() && !item.tier0_fired {
            match fire_tier0(&item).await {
                Ok(()) => {
                    item.tier0_fired = true;
                    item.updated_at_unix_ms = now_unix_ms;
                    write_item_and_audit(
                        db,
                        &item,
                        "tier0_toast_fired",
                        json!({ "suppress_popup": item.tier0_quiet_digest }),
                    )?;
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
                let ladder_index = item.ladder_index;
                let attempt_number = next_channel_attempt_number(&item, ladder_index);
                let attempt =
                    deliver_webhook(&channel, &item, ladder_index, attempt_number, now_unix_ms)
                        .await;
                if attempt.ok {
                    report.tier1_fired += 1;
                } else {
                    report.tier1_failed += 1;
                }
                let policy_window_ms = policy.window_for(item.severity);
                let retry_backoff_ms = (!attempt.ok
                    && attempt_number < WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL)
                    .then(|| webhook_retry_backoff_ms(attempt_number, policy_window_ms));
                let retry_exhausted = !attempt.ok && retry_backoff_ms.is_none();
                let event_detail = json!({
                    "channel_name": attempt.channel_name,
                    "ladder_index": ladder_index,
                    "attempt_number": attempt_number,
                    "max_attempts_per_channel": WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL,
                    "ok": attempt.ok,
                    "http_status": attempt.http_status,
                    "error": attempt.error,
                    "retry_backoff_ms": retry_backoff_ms,
                    "retry_exhausted": retry_exhausted,
                });
                item.channel_attempts.push(attempt);
                if item
                    .channel_attempts
                    .last()
                    .is_some_and(|attempt| attempt.ok || retry_exhausted)
                {
                    item.ladder_index += 1;
                }
                item.updated_at_unix_ms = now_unix_ms;
                item.next_escalate_at_unix_ms = retry_backoff_ms
                    .map(|backoff| now_unix_ms.saturating_add(backoff))
                    .or_else(|| {
                        ((item.ladder_index as usize) < policy.webhooks.len())
                            .then_some(now_unix_ms.saturating_add(policy_window_ms))
                    });
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

async fn remove_tier0_if_terminal(db: &Db, item: &mut EscalationItem) -> Result<bool, ErrorData> {
    if !item.tier0_fired || item.tier0_toast_removed.is_some() {
        return Ok(false);
    }
    let tag = escalation_toast_tag(&item.escalation_id);
    let outcome = remove_internal_toast(tag).await;
    let removed =
        outcome.removed || outcome.already_absent || outcome.status == "unsupported_platform";
    item.tier0_toast_removed = Some(outcome.clone());
    write_item_and_audit(
        db,
        item,
        "tier0_toast_removed",
        json!({
            "toast_removal": outcome,
        }),
    )?;
    tracing::info!(
        code = "ESCALATION_TIER0_TOAST_REMOVED",
        escalation_id = %item.escalation_id,
        status = %outcome.status,
        removed = outcome.removed,
        already_absent = outcome.already_absent,
        before_count = outcome.before_count,
        after_count = outcome.after_count,
        "readback=Action Center toast removal outcome stored"
    );
    Ok(removed)
}

fn tier0_removal_failed(item: &EscalationItem) -> bool {
    item.tier0_toast_removed.as_ref().is_some_and(|outcome| {
        !outcome.removed && !outcome.already_absent && outcome.status != "unsupported_platform"
    })
}

fn terminal_agent_read_for_item(
    terminal_agent_reads: &[AgentStateRead],
    item: &EscalationItem,
) -> Option<AgentStateRead> {
    terminal_agent_reads
        .iter()
        .find(|read| escalation_item_matches_agent_read(item, read))
        .cloned()
}

fn escalation_item_matches_agent_read(item: &EscalationItem, read: &AgentStateRead) -> bool {
    let anchor = item.anchor.as_str();
    if read.anchor == anchor
        || read.spawn_id.as_deref() == Some(anchor)
        || read.session_id.as_deref() == Some(anchor)
    {
        return true;
    }
    if let Some(spawn_id) = item.spawn_id.as_deref()
        && (read.anchor == spawn_id || read.spawn_id.as_deref() == Some(spawn_id))
    {
        return true;
    }
    if let Some(session_id) = item.session_id.as_deref()
        && (read.anchor == session_id || read.session_id.as_deref() == Some(session_id))
    {
        return true;
    }
    false
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
        dedupe_key: Some(escalation_toast_dedupe_key(&item.escalation_id)),
        suppress_popup: item.tier0_quiet_digest,
    };
    let tag = toast_tag_for(params.dedupe_key.as_deref());
    run_internal_toast(params, tag, Vec::new())
        .await
        .map(|_response| ())
}

fn escalation_toast_dedupe_key(escalation_id: &str) -> String {
    format!("escalation:{escalation_id}")
}

fn escalation_toast_tag(escalation_id: &str) -> String {
    toast_tag_for(Some(&escalation_toast_dedupe_key(escalation_id)))
}

async fn deliver_webhook(
    channel: &WebhookChannel,
    item: &EscalationItem,
    ladder_index: u32,
    attempt_number: u32,
    now_unix_ms: u64,
) -> ChannelAttempt {
    let url_host = reqwest::Url::parse(&channel.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unparseable>".to_owned());
    let mut attempt = ChannelAttempt {
        channel_name: channel.name.clone(),
        url_host,
        ladder_index,
        attempt_number,
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

fn next_channel_attempt_number(item: &EscalationItem, ladder_index: u32) -> u32 {
    let previous = item
        .channel_attempts
        .iter()
        .filter(|attempt| attempt.ladder_index == ladder_index)
        .count();
    u32::try_from(previous)
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

fn webhook_retry_backoff_ms(attempt_number: u32, policy_window_ms: u64) -> u64 {
    let capped_exponent = attempt_number.saturating_sub(1).min(4);
    let multiplier = 1_u64 << capped_exponent;
    let base = WEBHOOK_RETRY_BASE_BACKOFF_MS
        .min(policy_window_ms)
        .max(WORKER_TICK_MS);
    base.saturating_mul(multiplier)
        .min(WEBHOOK_RETRY_MAX_BACKOFF_MS)
        .min(policy_window_ms)
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

fn hex_encode_bytes(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
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
        let mut last_orphan_cleanup_unix_ms = 0_u64;
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
            let now_unix_ms = unix_time_ms_now();
            match process_pending(&db, now_unix_ms).await {
                Ok(report)
                    if report.tier0_fired
                        + report.tier0_removed
                        + report.tier0_remove_failed
                        + report.tier1_fired
                        + report.tier1_failed
                        + report.expired
                        + report.terminal_resolved
                        > 0 =>
                {
                    tracing::info!(
                        code = "ESCALATION_SWEEP",
                        tier0_fired = report.tier0_fired,
                        tier0_removed = report.tier0_removed,
                        tier0_remove_failed = report.tier0_remove_failed,
                        tier1_fired = report.tier1_fired,
                        tier1_failed = report.tier1_failed,
                        expired = report.expired,
                        terminal_resolved = report.terminal_resolved,
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
            if now_unix_ms.saturating_sub(last_orphan_cleanup_unix_ms) >= 60_000 {
                last_orphan_cleanup_unix_ms = now_unix_ms;
                let preserve_tags = match open_tier0_toast_tags(&db) {
                    Ok(tags) => tags,
                    Err(error) => {
                        tracing::error!(
                            code = "ESCALATION_ORPHAN_TOAST_PRESERVE_READ_FAILED",
                            detail = %error.message,
                            "could not read open escalation tags before orphan toast cleanup"
                        );
                        continue;
                    }
                };
                let report = remove_orphaned_escalation_toasts(preserve_tags).await;
                match write_orphan_toast_cleanup_audit(&db, &report, now_unix_ms) {
                    Ok(Some(row_key_hex)) => {
                        tracing::info!(
                            code = "ESCALATION_ORPHAN_TOAST_CLEANUP_AUDITED",
                            row_key_hex = %row_key_hex,
                            status = %report.status,
                            candidates = report.candidates,
                            removed = report.removed,
                            already_absent = report.already_absent,
                            preserved_open = report.preserved_open,
                            failed = report.failed,
                            "readback=CF_KV orphan escalation toast cleanup audit row"
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(
                            code = "ESCALATION_ORPHAN_TOAST_CLEANUP_AUDIT_FAILED",
                            detail = %error.message,
                            "orphan escalation toast cleanup ran but audit write/readback failed"
                        );
                    }
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
                format!(
                    "at most {MAX_WEBHOOKS} webhooks are allowed; got {}",
                    webhooks.len()
                ),
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
                    format!(
                        "webhook url for '{}' is not a valid URL: {error}",
                        channel.name
                    ),
                )
            })?;
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "webhook url for '{}' must be http:// or https://",
                        channel.name
                    ),
                ));
            }
            if let Some(secret) = &channel.secret
                && secret.chars().count() > MAX_SECRET_CHARS
            {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "webhook secret for '{}' must be <= {MAX_SECRET_CHARS} chars",
                        channel.name
                    ),
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
        prune_terminal_items(&db, unix_time_ms_now())?;
        let mut items = scan_items(&db)?;
        items.sort_by_key(|item| std::cmp::Reverse(item.created_at_unix_ms));
        let total_open = items.iter().filter(|item| item.status.is_open()).count();
        let limit = params.limit.unwrap_or(50).min(500) as usize;
        let filtered: Vec<EscalationItem> = items
            .into_iter()
            .filter(|item| params.status.map(|s| s == item.status).unwrap_or(true))
            .filter(|item| {
                params
                    .anchor
                    .as_deref()
                    .map(|a| a == item.anchor)
                    .unwrap_or(true)
            })
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

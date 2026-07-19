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
use std::time::{Duration, Instant};

use chrono::{Local, Timelike};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{SCHEMA_VERSION, error_codes};
use synapse_storage::{Db, RevisionGuard, RevisionedRawValue, cf, decode_json, encode_json};
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
use crate::m3::grounding::{self, SOURCE_ESCALATION};

type CfKvRow = (Vec<u8>, Vec<u8>);
type CfKvRows = Vec<CfKvRow>;

#[derive(Clone, Debug, Default)]
struct GuardedExtraRows {
    rows: CfKvRows,
    guards: Vec<RevisionGuard>,
}

impl GuardedExtraRows {
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn extend(&mut self, other: Self) {
        self.rows.extend(other.rows);
        self.guards.extend(other.guards);
    }
}

const CONFIG_KEY: &str = "escalation/v1/config";
const ITEM_PREFIX: &str = "escalation/v1/item/";
const AUDIT_PREFIX: &str = "escalation/v1/audit/";
const OPEN_INDEX_PREFIX: &str = "escalation/v1/open/";
const ORPHAN_TOAST_AUDIT_PREFIX: &str = "escalation/v1/toast_orphan_cleanup/";
const ESCALATION_ID_PREFIX: &str = "esc1-";
const ESCALATION_ID_HEX_LEN: usize = 32;
const ESCALATION_ID_LEN: usize = ESCALATION_ID_PREFIX.len() + ESCALATION_ID_HEX_LEN;
const ITEM_KEY_LEN: usize = ITEM_PREFIX.len() + ESCALATION_ID_LEN;
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
const WORKER_COOPERATIVE_YIELD_ROWS: usize = 32;
const WORKER_SLOW_SCAN_LOG_MS: u128 = 500;
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
const ACK_REVISION_MAX_ATTEMPTS: usize = 16;
const DELETE_REVISION_MAX_ATTEMPTS: usize = 16;
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

fn open_index_key(anchor: &str, attention_state: &str) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(b"synapse.escalation.open-index.v1\0");
    digest.update(
        u64::try_from(anchor.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(anchor.as_bytes());
    digest.update(
        u64::try_from(attention_state.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(attention_state.as_bytes());
    format!("{OPEN_INDEX_PREFIX}{}", hex_bytes(&digest.finalize())).into_bytes()
}

fn orphan_toast_audit_key(at_unix_ms: u64, event_id: &str) -> Vec<u8> {
    format!("{ORPHAN_TOAST_AUDIT_PREFIX}{at_unix_ms:020}-{event_id}").into_bytes()
}

fn storage_error(error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

fn live_revisioned_value<'a>(
    row: &'a RevisionedRawValue,
    identity: &str,
) -> Result<&'a [u8], ErrorData> {
    row.value.as_deref().ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "{identity} has a physical Calyx revision but its retention envelope is expired; refusing to treat stale control-plane bytes as live"
            ),
        )
    })
}

fn validate_open_index_record(
    key: &[u8],
    record: &OpenEscalationIndexRecord,
) -> Result<(), ErrorData> {
    validate_escalation_id(&record.escalation_id)?;
    if record.schema_version != SCHEMA_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "open escalation index has schema_version={} instead of {}: escalation_id={}",
                record.schema_version, SCHEMA_VERSION, record.escalation_id
            ),
        ));
    }
    let expected_key = open_index_key(&record.anchor, &record.attention_state);
    if key != expected_key {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "open escalation index key/payload identity mismatch: stored_key={} expected_key={} escalation_id={}",
                hex_bytes(key),
                hex_bytes(&expected_key),
                record.escalation_id
            ),
        ));
    }
    Ok(())
}

fn read_open_index(
    db: &Db,
    anchor: &str,
    attention_state: &str,
) -> Result<Option<RevisionedOpenEscalationIndex>, ErrorData> {
    let key = open_index_key(anchor, attention_state);
    db.get_cf_revisioned(cf::CF_KV, &key)
        .map_err(storage_error)?
        .map(|revisioned| {
            let record = decode_json::<OpenEscalationIndexRecord>(live_revisioned_value(
                &revisioned,
                &format!("open escalation index {}", hex_bytes(&key)),
            )?)
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "open escalation index decode failed for key {}: {error}",
                        hex_bytes(&key)
                    ),
                )
            })?;
            validate_open_index_record(&key, &record)?;
            if record.anchor != anchor || record.attention_state != attention_state {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "open escalation index digest collision or identity mismatch: requested_anchor={anchor:?} requested_state={attention_state:?} stored_anchor={:?} stored_state={:?}",
                        record.anchor, record.attention_state
                    ),
                ));
            }
            Ok(RevisionedOpenEscalationIndex {
                record,
                revision_sha256: revisioned.revision_sha256,
            })
        })
        .transpose()
}

fn open_index_row(
    item: &EscalationItem,
    is_open: bool,
    expected_revision_sha256: Option<[u8; 32]>,
) -> Result<GuardedExtraRows, ErrorData> {
    let key = open_index_key(&item.anchor, &item.attention_state);
    let record = OpenEscalationIndexRecord {
        schema_version: SCHEMA_VERSION,
        anchor: item.anchor.clone(),
        attention_state: item.attention_state.clone(),
        escalation_id: item.escalation_id.clone(),
        is_open,
        updated_at_unix_ms: item.updated_at_unix_ms,
    };
    validate_open_index_record(&key, &record)?;
    let value = encode_json(&record).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "open escalation index encode failed for {}: {error}",
                item.escalation_id
            ),
        )
    })?;
    Ok(GuardedExtraRows {
        rows: vec![(key.clone(), value)],
        guards: vec![RevisionGuard::new(key, expected_revision_sha256)],
    })
}

fn indexed_open_item(
    db: &Db,
    index: &RevisionedOpenEscalationIndex,
) -> Result<EscalationItem, ErrorData> {
    if !index.record.is_open {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "terminal escalation index was requested as open: escalation_id={}",
                index.record.escalation_id
            ),
        ));
    }
    let item = read_item(db, &index.record.escalation_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "open escalation index points to a missing item: escalation_id={}",
                index.record.escalation_id
            ),
        )
    })?;
    if !item.status.is_open()
        || item.anchor != index.record.anchor
        || item.attention_state != index.record.attention_state
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "open escalation index points to a non-open or mismatched item: escalation_id={} item_status={} item_anchor={:?} index_anchor={:?} item_state={:?} index_state={:?}",
                item.escalation_id,
                item.status.as_str(),
                item.anchor,
                index.record.anchor,
                item.attention_state,
                index.record.attention_state
            ),
        ));
    }
    Ok(item)
}

fn terminal_open_index_row(db: &Db, item: &EscalationItem) -> Result<GuardedExtraRows, ErrorData> {
    let current = read_open_index(db, &item.anchor, &item.attention_state)?;
    match current {
        None => open_index_row(item, false, None),
        Some(current) if current.record.escalation_id == item.escalation_id => {
            if current.record.is_open {
                open_index_row(item, false, Some(current.revision_sha256))
            } else {
                Ok(GuardedExtraRows::default())
            }
        }
        Some(current) => {
            // A later generation may already own this deterministic slot while
            // a worker is revisiting an older terminal item. Validate any open
            // owner, then leave its generation untouched.
            if current.record.is_open {
                indexed_open_item(db, &current)?;
            }
            Ok(GuardedExtraRows::default())
        }
    }
}

fn validate_escalation_id(escalation_id: &str) -> Result<(), ErrorData> {
    escalation_id
        .strip_prefix(ESCALATION_ID_PREFIX)
        .filter(|hex| {
            hex.len() == ESCALATION_ID_HEX_LEN
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .map(|_hex| ())
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "escalation id {escalation_id:?} is not canonical {ESCALATION_ID_PREFIX}<32 lowercase hex>"
                ),
            )
        })
}

fn encode_item(item: &EscalationItem) -> Result<Vec<u8>, ErrorData> {
    validate_escalation_id(&item.escalation_id)?;
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

#[derive(Clone, Debug)]
struct RevisionedEscalationItem {
    item: EscalationItem,
    revision_sha256: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenEscalationIndexRecord {
    schema_version: u32,
    anchor: String,
    attention_state: String,
    escalation_id: String,
    is_open: bool,
    updated_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct RevisionedOpenEscalationIndex {
    record: OpenEscalationIndexRecord,
    revision_sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ItemWriteGuard {
    Absent,
    Revision([u8; 32]),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ItemWriteOutcome {
    Applied {
        revision_sha256: [u8; 32],
        committed_seq: u64,
    },
    Conflict {
        actual_revision_sha256: Option<[u8; 32]>,
        observed_seq: u64,
    },
}

fn accept_applied_item_revision(
    outcome: ItemWriteOutcome,
    revision_sha256: &mut [u8; 32],
) -> Option<u64> {
    match outcome {
        ItemWriteOutcome::Applied {
            revision_sha256: committed_revision,
            committed_seq,
        } => {
            *revision_sha256 = committed_revision;
            Some(committed_seq)
        }
        ItemWriteOutcome::Conflict { .. } => None,
    }
}

/// Creates an absent escalation item plus its append-only audit row in one
/// revision-guarded pressure-bypass batch.
fn create_item_and_audit_with_extra_rows(
    db: &Db,
    item: &EscalationItem,
    event: &str,
    detail: Value,
    extra_rows: GuardedExtraRows,
) -> Result<ItemWriteOutcome, ErrorData> {
    write_item_and_audit_guarded(db, item, event, detail, extra_rows, ItemWriteGuard::Absent)
}

fn write_item_and_audit_if_revision(
    db: &Db,
    item: &EscalationItem,
    event: &str,
    detail: Value,
    expected_revision_sha256: [u8; 32],
) -> Result<ItemWriteOutcome, ErrorData> {
    write_item_and_audit_with_extra_rows_if_revision(
        db,
        item,
        event,
        detail,
        GuardedExtraRows::default(),
        expected_revision_sha256,
    )
}

fn write_item_and_audit_with_extra_rows_if_revision(
    db: &Db,
    item: &EscalationItem,
    event: &str,
    detail: Value,
    extra_rows: GuardedExtraRows,
    expected_revision_sha256: [u8; 32],
) -> Result<ItemWriteOutcome, ErrorData> {
    write_item_and_audit_guarded(
        db,
        item,
        event,
        detail,
        extra_rows,
        ItemWriteGuard::Revision(expected_revision_sha256),
    )
}

fn write_item_and_audit_guarded(
    db: &Db,
    item: &EscalationItem,
    event: &str,
    detail: Value,
    extra_rows: GuardedExtraRows,
    guard: ItemWriteGuard,
) -> Result<ItemWriteOutcome, ErrorData> {
    let GuardedExtraRows {
        rows: extra_rows,
        guards: extra_guards,
    } = extra_rows;
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
        (item_key.clone(), item_value.clone()),
        (audit_key.clone(), audit_value.clone()),
    ];
    rows.extend(extra_rows.iter().cloned());
    let expected_revision_sha256 = match guard {
        ItemWriteGuard::Absent => None,
        ItemWriteGuard::Revision(revision) => Some(revision),
    };
    let mut revision_guards = Vec::with_capacity(1 + extra_guards.len());
    revision_guards.push(RevisionGuard::new(
        item_key.clone(),
        expected_revision_sha256,
    ));
    revision_guards.extend(extra_guards);
    let outcome = db
        .mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            revision_guards.clone(),
            std::iter::empty::<Vec<u8>>(),
            rows,
        )
        .map_err(storage_error)?;
    if !outcome.applied {
        let conflict = outcome.conflict.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "multi-key escalation mutation reported applied=false without conflict detail: escalation_id={} event={} observed_seq={}",
                    item.escalation_id, event, outcome.committed_seq
                ),
            )
        })?;
        tracing::warn!(
            code = "ESCALATION_MUTATION_REVISION_CONFLICT",
            escalation_id = %item.escalation_id,
            event,
            observed_seq = outcome.committed_seq,
            conflict_guard_index = conflict.guard_index,
            conflict_guard_key = %hex_bytes(&conflict.key),
            expected_revision = conflict.expected_revision_sha256
                .as_ref()
                .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision)),
            actual_revision = conflict
                .actual_revision_sha256
                .as_ref()
                .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision)),
            "multi-key revision-guarded escalation mutation was not applied; caller must reread authoritative item and approval state"
        );
        return Ok(ItemWriteOutcome::Conflict {
            actual_revision_sha256: conflict.actual_revision_sha256,
            observed_seq: outcome.committed_seq,
        });
    }
    if outcome.committed_revisions_sha256.len() != revision_guards.len() {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "multi-key escalation mutation returned invalid committed revision shape: escalation_id={} event={} guards={} revisions={} committed_seq={}",
                item.escalation_id,
                event,
                revision_guards.len(),
                outcome.committed_revisions_sha256.len(),
                outcome.committed_seq
            ),
        ));
    }
    let committed_revision_sha256 = outcome.committed_revisions_sha256[0].ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "revision-guarded escalation write committed without guarded-row revision readback: escalation_id={} event={} committed_seq={}",
                item.escalation_id, event, outcome.committed_seq
            ),
        )
    })?;
    // Physical write-readback guard: prove both rows are present immediately.
    let item_readback = db
        .get_cf_revisioned(cf::CF_KV, &item_key)
        .map_err(storage_error)?
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "escalation item row absent immediately after write for {}",
                    item.escalation_id
                ),
            )
        })?;
    let item_readback_value = live_revisioned_value(
        &item_readback,
        &format!("escalation item {}", item.escalation_id),
    )?;
    if item_readback.revision_sha256 == committed_revision_sha256
        && item_readback_value != item_value
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation item revision readback matched committed revision but bytes differed: escalation_id={} event={} committed_seq={}",
                item.escalation_id, event, outcome.committed_seq
            ),
        ));
    }
    if item_readback.revision_sha256 != committed_revision_sha256 {
        let superseding_item = decode_item(&item.escalation_id, item_readback_value)?;
        tracing::info!(
            code = "ESCALATION_ITEM_READBACK_SUPERSEDED",
            escalation_id = %item.escalation_id,
            event,
            committed_seq = outcome.committed_seq,
            committed_revision = %hex_bytes(&committed_revision_sha256),
            latest_revision = %hex_bytes(&item_readback.revision_sha256),
            latest_status = superseding_item.status.as_str(),
            "separate latest readback decoded and identity-validated a newer item revision after the guarded commit"
        );
    }
    let audit_readback_value = read_exact_row(db, &audit_key)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "escalation audit row absent immediately after write for {}",
                item.escalation_id
            ),
        )
    })?;
    if audit_readback_value != audit_value {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation audit row bytes differed immediately after write: escalation_id={} event={} key={} committed_seq={}",
                item.escalation_id,
                event,
                hex_bytes(&audit_key),
                outcome.committed_seq
            ),
        ));
    }
    for (key, expected_value) in &extra_rows {
        let guard_index = revision_guards.iter().position(|guard| guard.key == *key);
        if let Some(guard_index) = guard_index {
            let committed_revision = outcome.committed_revisions_sha256[guard_index].ok_or_else(
                || {
                    mcp_error(
                        error_codes::STORAGE_WRITE_FAILED,
                        format!(
                            "guarded linked row committed as absent despite a put: escalation_id={} event={} guard_index={} key={} committed_seq={}",
                            item.escalation_id,
                            event,
                            guard_index,
                            hex_bytes(key),
                            outcome.committed_seq
                        ),
                    )
                },
            )?;
            let readback = db
                .get_cf_revisioned(cf::CF_KV, key)
                .map_err(storage_error)?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "guarded linked row absent immediately after write: escalation_id={} event={} key={}",
                            item.escalation_id,
                            event,
                            hex_bytes(key)
                        ),
                    )
                })?;
            let readback_value = live_revisioned_value(
                &readback,
                &format!(
                    "linked escalation row {} for {}",
                    hex_bytes(key),
                    item.escalation_id
                ),
            )?;
            if readback.revision_sha256 == committed_revision {
                if readback_value != expected_value.as_slice() {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "guarded linked row revision matched but bytes differed: escalation_id={} event={} key={} committed_seq={}",
                            item.escalation_id,
                            event,
                            hex_bytes(key),
                            outcome.committed_seq
                        ),
                    ));
                }
            } else {
                validate_guarded_extra_row_identity(key, readback_value)?;
                tracing::info!(
                    code = "ESCALATION_GUARDED_EXTRA_READBACK_SUPERSEDED",
                    escalation_id = %item.escalation_id,
                    event,
                    committed_seq = outcome.committed_seq,
                    key = %hex_bytes(key),
                    committed_revision = %hex_bytes(&committed_revision),
                    latest_revision = %hex_bytes(&readback.revision_sha256),
                    "separate latest readback decoded and identity-validated a newer guarded extra-row revision"
                );
            }
        } else {
            let readback = read_exact_row(db, key)?.ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "linked audit row absent immediately after write: escalation_id={} event={} key={}",
                        item.escalation_id,
                        event,
                        hex_bytes(key)
                    ),
                )
            })?;
            if readback != *expected_value {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "linked audit row bytes differed immediately after write: escalation_id={} event={} key={} committed_seq={}",
                        item.escalation_id,
                        event,
                        hex_bytes(key),
                        outcome.committed_seq
                    ),
                ));
            }
        }
    }
    let audit_readback: Value = decode_json(&audit_readback_value).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "escalation audit row decode failed immediately after write for {}: {error}",
                item.escalation_id
            ),
        )
    })?;
    let report = grounding::write_outcome_constellation_and_anchor(
        db,
        cf::CF_KV,
        &audit_key,
        &audit_readback_value,
        &audit_readback,
        grounding::enum_anchor("synapse:escalation_event", event, SOURCE_ESCALATION, at),
        "escalation audit anchor",
    )?;
    tracing::info!(
        code = "ESCALATION_EVENT_ANCHORED",
        escalation_id = %item.escalation_id,
        event,
        status = item.status.as_str(),
        source_key = %String::from_utf8_lossy(&audit_key),
        cx_id = %report.cx_id,
        ledger_seq = report.ledger_seq,
        "escalation audit outcome grounded on CF_KV audit constellation"
    );
    Ok(ItemWriteOutcome::Applied {
        revision_sha256: committed_revision_sha256,
        committed_seq: outcome.committed_seq,
    })
}

fn approval_item_key(approval_id: &str) -> Vec<u8> {
    format!("{APPROVAL_ITEM_PREFIX}{approval_id}").into_bytes()
}

fn approval_audit_key(approval_id: &str, at_unix_ms: u64, event_id: &str) -> Vec<u8> {
    format!("{APPROVAL_AUDIT_PREFIX}{approval_id}/{at_unix_ms:020}-{event_id}").into_bytes()
}

fn validate_linked_approval_item_identity(
    key: &[u8],
    value: &[u8],
) -> Result<ApprovalItemRecord, ErrorData> {
    let key_text = std::str::from_utf8(key).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "linked approval item key is not UTF-8: key={} error={error}",
                hex_bytes(key)
            ),
        )
    })?;
    let approval_id = key_text
        .strip_prefix(APPROVAL_ITEM_PREFIX)
        .filter(|approval_id| !approval_id.is_empty())
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "guarded linked row is not a canonical approval item key: key={}",
                    hex_bytes(key)
                ),
            )
        })?;
    let approval = decode_json::<ApprovalItemRecord>(value).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "linked approval item decode failed during revision readback: approval_id={approval_id} error={error}"
            ),
        )
    })?;
    if approval.approval_id != approval_id || approval.kind != ApprovalKind::AgentEscalation {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "linked approval row/payload identity mismatch: row_approval_id={approval_id:?} payload_approval_id={:?} payload_kind={:?}",
                approval.approval_id, approval.kind
            ),
        ));
    }
    Ok(approval)
}

fn validate_linked_approval_for_escalation(
    item: &EscalationItem,
    approval: &ApprovalItemRecord,
) -> Result<(), ErrorData> {
    let payload_json = approval.payload_json.as_deref().ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "linked approval {} for escalation {} has no payload_json",
                approval.approval_id, item.escalation_id
            ),
        )
    })?;
    let payload = serde_json::from_str::<Value>(payload_json).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "linked approval {} payload_json is invalid for escalation {}: {error}",
                approval.approval_id, item.escalation_id
            ),
        )
    })?;
    let payload_escalation_id = payload.get("escalation_id").and_then(Value::as_str);
    let payload_anchor = payload.get("anchor").and_then(Value::as_str);
    let payload_attention_state = payload.get("attention_state").and_then(Value::as_str);
    if approval.approval_id != item.approval_id
        || approval.kind != ApprovalKind::AgentEscalation
        || payload_escalation_id != Some(item.escalation_id.as_str())
        || payload_anchor != Some(item.anchor.as_str())
        || payload_attention_state != Some(item.attention_state.as_str())
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "linked approval/escalation identity mismatch: item_escalation_id={:?} item_approval_id={:?} item_anchor={:?} item_state={:?} approval_id={:?} approval_kind={:?} payload_escalation_id={payload_escalation_id:?} payload_anchor={payload_anchor:?} payload_state={payload_attention_state:?}",
                item.escalation_id,
                item.approval_id,
                item.anchor,
                item.attention_state,
                approval.approval_id,
                approval.kind
            ),
        ));
    }
    Ok(())
}

fn validate_guarded_extra_row_identity(key: &[u8], value: &[u8]) -> Result<(), ErrorData> {
    if key.starts_with(APPROVAL_ITEM_PREFIX.as_bytes()) {
        validate_linked_approval_item_identity(key, value)?;
        return Ok(());
    }
    if key.starts_with(OPEN_INDEX_PREFIX.as_bytes()) {
        let record = decode_json::<OpenEscalationIndexRecord>(value).map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "open escalation index decode failed during revision readback: key={} error={error}",
                    hex_bytes(key)
                ),
            )
        })?;
        return validate_open_index_record(key, &record);
    }
    Err(mcp_error(
        error_codes::STORAGE_CORRUPTED,
        format!(
            "guarded escalation extra row has no registered identity validator: key={}",
            hex_bytes(key)
        ),
    ))
}

fn read_exact_row(db: &Db, key: &[u8]) -> Result<Option<Vec<u8>>, ErrorData> {
    db.get_cf(cf::CF_KV, key).map_err(storage_error)
}

fn read_item(db: &Db, escalation_id: &str) -> Result<Option<EscalationItem>, ErrorData> {
    read_item_revisioned(db, escalation_id).map(|item| item.map(|revisioned| revisioned.item))
}

fn read_item_revisioned(
    db: &Db,
    escalation_id: &str,
) -> Result<Option<RevisionedEscalationItem>, ErrorData> {
    validate_escalation_id(escalation_id)?;
    let key = item_key(escalation_id);
    db.get_cf_revisioned(cf::CF_KV, &key)
        .map_err(storage_error)?
        .map(|revisioned| {
            Ok(RevisionedEscalationItem {
                item: decode_item(
                    escalation_id,
                    live_revisioned_value(
                        &revisioned,
                        &format!("escalation item {escalation_id}"),
                    )?,
                )?,
                revision_sha256: revisioned.revision_sha256,
            })
        })
        .transpose()
}

fn decode_item(escalation_id: &str, value: &[u8]) -> Result<EscalationItem, ErrorData> {
    validate_escalation_id(escalation_id)?;
    let item = decode_json::<EscalationItem>(value).map_err(|error| {
        mcp_error(
            error.code(),
            format!("escalation item decode failed for {escalation_id}: {error}"),
        )
    })?;
    if item.escalation_id != escalation_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation item row/payload identity mismatch: row={escalation_id:?} payload={:?}",
                item.escalation_id
            ),
        ));
    }
    Ok(item)
}

#[derive(Clone, Debug)]
struct EscalationItemRow {
    key: Vec<u8>,
    item: EscalationItem,
}

fn item_scan_bounds() -> (Vec<u8>, Vec<u8>) {
    let mut start = ITEM_PREFIX.as_bytes().to_vec();
    start.resize(ITEM_KEY_LEN, 0);
    let mut end = ITEM_PREFIX.as_bytes().to_vec();
    for byte in end.iter_mut().rev() {
        if *byte != u8::MAX {
            *byte += 1;
            end.resize(ITEM_KEY_LEN, 0);
            return (start, end);
        }
        *byte = 0;
    }
    unreachable!("ASCII escalation item prefix always has a lexicographic successor")
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn scan_item_page(
    db: &Db,
    start: &[u8],
    end: &[u8],
    after_key: Option<&[u8]>,
) -> Result<synapse_storage::FixedWidthScanPage, ErrorData> {
    db.scan_cf_fixed_width_range_page(cf::CF_KV, start, end, after_key, SCAN_CHUNK_ROWS)
        .map_err(storage_error)
}

#[derive(Clone, Copy, Debug, Default)]
struct ItemPageSnapshotTelemetry {
    first: Option<u64>,
    last: Option<u64>,
    changes: usize,
}

impl ItemPageSnapshotTelemetry {
    fn observe(&mut self, actual: Option<u64>) -> Result<(), ErrorData> {
        let actual = actual.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                "escalation fixed-width page omitted its atomic Calyx snapshot sequence",
            )
        })?;
        if self.last.is_some_and(|previous| previous != actual) {
            self.changes = self.changes.saturating_add(1);
        }
        self.first.get_or_insert(actual);
        self.last = Some(actual);
        Ok(())
    }
}

fn decode_item_page(rows: Vec<(Vec<u8>, Vec<u8>)>) -> Result<Vec<EscalationItemRow>, ErrorData> {
    let mut decoded = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        if key.len() != ITEM_KEY_LEN || !key.starts_with(ITEM_PREFIX.as_bytes()) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "fixed-width escalation range returned an out-of-contract row: expected {ITEM_KEY_LEN} bytes beginning with {ITEM_PREFIX:?}, got len={} key_hex={}",
                    key.len(),
                    hex_bytes(&key)
                ),
            ));
        }
        let key_text = std::str::from_utf8(&key).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "fixed-width escalation row key is not UTF-8: key_hex={} error={error}",
                    hex_bytes(&key)
                ),
            )
        })?;
        let id = key_text.strip_prefix(ITEM_PREFIX).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "fixed-width escalation row key lost required prefix {ITEM_PREFIX:?}: key_hex={}",
                    hex_bytes(&key)
                ),
            )
        })?;
        let item = decode_item(id, &value)?;
        decoded.push(EscalationItemRow { key, item });
    }
    Ok(decoded)
}

fn next_item_page_cursor(
    page: &synapse_storage::FixedWidthScanPage,
    previous: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, ErrorData> {
    if !page.more {
        return Ok(None);
    }
    let cursor = page.resume_after.clone().ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            "escalation fixed-width page reported more candidates without a resume cursor",
        )
    })?;
    if previous.is_some_and(|previous| cursor.as_slice() <= previous) {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "escalation fixed-width page returned a non-progressing cursor: previous={} current={}",
                previous.map_or_else(|| "none".to_owned(), hex_bytes),
                hex_bytes(&cursor)
            ),
        ));
    }
    Ok(Some(cursor))
}

fn scan_item_rows(db: &Db) -> Result<Vec<EscalationItemRow>, ErrorData> {
    let (start, end) = item_scan_bounds();
    let mut after_key = None;
    let mut snapshot_telemetry = ItemPageSnapshotTelemetry::default();
    let mut out = Vec::new();
    let mut pages = 0usize;
    loop {
        let page = scan_item_page(db, &start, &end, after_key.as_deref())?;
        pages = pages.saturating_add(1);
        snapshot_telemetry.observe(page.snapshot_seq)?;
        let next = next_item_page_cursor(&page, after_key.as_deref())?;
        out.extend(decode_item_page(page.rows)?);
        let Some(cursor) = next else {
            break;
        };
        after_key = Some(cursor);
    }
    tracing::debug!(
        code = "ESCALATION_ITEM_SCAN_PAGE_SNAPSHOTS",
        scan_mode = "synchronous",
        scan_pages = pages,
        first_snapshot_seq = snapshot_telemetry.first,
        last_snapshot_seq = snapshot_telemetry.last,
        snapshot_seq_changes = snapshot_telemetry.changes,
        "completed ordered escalation item paging across per-page atomic snapshots"
    );
    Ok(out)
}

struct CancellableItemScan {
    rows: Vec<EscalationItemRow>,
    pages: usize,
    candidate_rows_examined: usize,
    expired_rows_skipped: usize,
    first_snapshot_seq: Option<u64>,
    last_snapshot_seq: Option<u64>,
    snapshot_seq_changes: usize,
    elapsed_ms: u128,
}

async fn scan_item_rows_cancellable(
    db: &Db,
    shutdown: &CancellationToken,
) -> Result<Option<CancellableItemScan>, ErrorData> {
    let started = Instant::now();
    let (start, end) = item_scan_bounds();
    let mut after_key = None;
    let mut snapshot_telemetry = ItemPageSnapshotTelemetry::default();
    let mut rows = Vec::new();
    let mut pages = 0usize;
    let mut candidate_rows_examined = 0usize;
    let mut expired_rows_skipped = 0usize;
    loop {
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        let page_started = Instant::now();
        let page = scan_item_page(db, &start, &end, after_key.as_deref())?;
        pages = pages.saturating_add(1);
        candidate_rows_examined =
            candidate_rows_examined.saturating_add(page.candidate_rows_examined);
        expired_rows_skipped = expired_rows_skipped.saturating_add(page.expired_rows_skipped);
        snapshot_telemetry.observe(page.snapshot_seq)?;
        let next = next_item_page_cursor(&page, after_key.as_deref())?;
        rows.extend(decode_item_page(page.rows)?);
        let page_elapsed_ms = page_started.elapsed().as_millis();
        if page_elapsed_ms >= WORKER_SLOW_SCAN_LOG_MS {
            tracing::warn!(
                code = "ESCALATION_ITEM_PAGE_SLOW",
                page_elapsed_ms,
                page_number = pages,
                candidate_rows_examined = page.candidate_rows_examined,
                expired_rows_skipped = page.expired_rows_skipped,
                page_more = page.more,
                page_snapshot_seq = page.snapshot_seq,
                first_snapshot_seq = snapshot_telemetry.first,
                snapshot_seq_changes = snapshot_telemetry.changes,
                requested_candidate_rows = SCAN_CHUNK_ROWS,
                item_key_len = ITEM_KEY_LEN,
                "candidate-bounded escalation item page exceeded the latency budget"
            );
        }
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        let Some(cursor) = next else {
            break;
        };
        after_key = Some(cursor);
        tokio::task::yield_now().await;
    }
    tracing::debug!(
        code = "ESCALATION_ITEM_SCAN_PAGE_SNAPSHOTS",
        scan_mode = "cancellable",
        scan_pages = pages,
        first_snapshot_seq = snapshot_telemetry.first,
        last_snapshot_seq = snapshot_telemetry.last,
        snapshot_seq_changes = snapshot_telemetry.changes,
        "completed ordered escalation item paging across per-page atomic snapshots"
    );
    Ok(Some(CancellableItemScan {
        rows,
        pages,
        candidate_rows_examined,
        expired_rows_skipped,
        first_snapshot_seq: snapshot_telemetry.first,
        last_snapshot_seq: snapshot_telemetry.last,
        snapshot_seq_changes: snapshot_telemetry.changes,
        elapsed_ms: started.elapsed().as_millis(),
    }))
}

#[derive(Clone, Debug)]
struct TerminalItemDeleteCandidate {
    key: Vec<u8>,
    escalation_id: String,
    scanned_updated_at_unix_ms: u64,
}

fn current_terminal_delete_guard(
    db: &Db,
    candidate: &TerminalItemDeleteCandidate,
) -> Result<Option<RevisionGuard>, ErrorData> {
    let Some(revisioned) = db
        .get_cf_revisioned(cf::CF_KV, &candidate.key)
        .map_err(storage_error)?
    else {
        return Ok(None);
    };
    let mut item = decode_item(
        &candidate.escalation_id,
        live_revisioned_value(
            &revisioned,
            &format!("terminal escalation item {}", candidate.escalation_id),
        )?,
    )?;
    if item.status.is_open() || item.updated_at_unix_ms != candidate.scanned_updated_at_unix_ms {
        tracing::info!(
            code = "ESCALATION_RETENTION_CANDIDATE_SUPERSEDED",
            escalation_id = %candidate.escalation_id,
            scanned_updated_at_unix_ms = candidate.scanned_updated_at_unix_ms,
            latest_updated_at_unix_ms = item.updated_at_unix_ms,
            latest_status = item.status.as_str(),
            "terminal retention candidate changed before its guarded delete boundary and was not deleted"
        );
        return Ok(None);
    }
    item.updated_at_unix_ms = unix_time_ms_now();
    let mut prerequisite_rows = linked_approval_terminal_rows(
        db,
        &item,
        "linked_escalation_retention_prerequisite",
        format!(
            "linked escalation {} must have a terminal approval before item retention deletion",
            item.escalation_id
        ),
    )?;
    let approval_repaired = !prerequisite_rows.is_empty();
    prerequisite_rows.extend(terminal_open_index_row(db, &item)?);
    if !prerequisite_rows.is_empty() {
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            "retention_prerequisite_repaired",
            json!({
                "approval_repaired": approval_repaired,
                "open_index_repaired": true,
            }),
            prerequisite_rows,
            revisioned.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_RETENTION_PREREQUISITE_REPAIRED",
                    escalation_id = %item.escalation_id,
                    approval_repaired,
                    committed_seq,
                    "terminal escalation retained for another sweep after atomically repairing linked approval/open-index prerequisites"
                );
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_RETENTION_PREREQUISITE_CONFLICT",
                    escalation_id = %item.escalation_id,
                    observed_seq,
                    "terminal escalation changed while repairing deletion prerequisites and was not deleted"
                );
            }
        }
        return Ok(None);
    }
    Ok(Some(RevisionGuard::new(
        candidate.key.clone(),
        Some(revisioned.revision_sha256),
    )))
}

fn delete_terminal_candidate_chunk(
    db: &Db,
    candidates: &[TerminalItemDeleteCandidate],
    context: &str,
    shutdown: Option<&CancellationToken>,
) -> Result<Option<usize>, ErrorData> {
    if shutdown.is_some_and(CancellationToken::is_cancelled) {
        return Ok(None);
    }
    let mut pending = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if let Some(guard) = current_terminal_delete_guard(db, candidate)? {
            pending.push((candidate.clone(), guard));
        }
    }
    if pending.is_empty() {
        return Ok(Some(0));
    }

    for attempt in 1..=DELETE_REVISION_MAX_ATTEMPTS {
        if shutdown.is_some_and(CancellationToken::is_cancelled) {
            return Ok(None);
        }
        let guards = pending
            .iter()
            .map(|(_candidate, guard)| guard.clone())
            .collect::<Vec<_>>();
        let keys = pending
            .iter()
            .map(|(candidate, _guard)| candidate.key.clone())
            .collect::<Vec<_>>();
        let outcome = db
            .mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                guards,
                keys.iter().cloned(),
                std::iter::empty::<(Vec<u8>, Vec<u8>)>(),
            )
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("{context} failed to revision-guard terminal row deletion: {error}"),
                )
            })?;
        if outcome.applied {
            if outcome.committed_revisions_sha256.len() != keys.len()
                || outcome
                    .committed_revisions_sha256
                    .iter()
                    .any(Option::is_some)
            {
                return Err(mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "{context} returned invalid delete revision readback: keys={} revisions={} non_absent={}",
                        keys.len(),
                        outcome.committed_revisions_sha256.len(),
                        outcome
                            .committed_revisions_sha256
                            .iter()
                            .filter(|revision| revision.is_some())
                            .count()
                    ),
                ));
            }
            for key in &keys {
                if read_exact_row(db, key)?.is_some() {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "{context} guarded delete read back a live row: key={} committed_seq={}",
                            hex_bytes(key),
                            outcome.committed_seq
                        ),
                    ));
                }
            }
            tracing::info!(
                code = "ESCALATION_TERMINAL_DELETE_READBACK",
                context,
                committed_seq = outcome.committed_seq,
                deleted_rows = keys.len(),
                first_key = %keys.first().map_or_else(|| "none".to_owned(), |key| hex_bytes(key)),
                last_key = %keys.last().map_or_else(|| "none".to_owned(), |key| hex_bytes(key)),
                "readback=CF_KV every revision-guarded terminal row is absent"
            );
            return Ok(Some(keys.len()));
        }

        let conflict = outcome.conflict.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "{context} returned applied=false without revision conflict detail at attempt {attempt}"
                ),
            )
        })?;
        if conflict.guard_index >= pending.len()
            || pending[conflict.guard_index].0.key != conflict.key
        {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "{context} returned out-of-contract conflict: guard_index={} pending={} conflict_key={}",
                    conflict.guard_index,
                    pending.len(),
                    hex_bytes(&conflict.key)
                ),
            ));
        }
        let (candidate, _stale_guard) = pending.remove(conflict.guard_index);
        if let Some(refreshed_guard) = current_terminal_delete_guard(db, &candidate)? {
            pending.insert(conflict.guard_index, (candidate, refreshed_guard));
        }
        tracing::info!(
            code = "ESCALATION_TERMINAL_DELETE_REVISION_RETRY",
            context,
            attempt,
            max_attempts = DELETE_REVISION_MAX_ATTEMPTS,
            conflict_guard_index = conflict.guard_index,
            conflict_key = %hex_bytes(&conflict.key),
            remaining_candidates = pending.len(),
            "terminal retention lost a revision race; no row was deleted and the authoritative candidate set was refreshed"
        );
        if pending.is_empty() {
            return Ok(Some(0));
        }
    }

    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "{context} could not acquire a stable terminal-row revision set after {DELETE_REVISION_MAX_ATTEMPTS} attempts; inspect ESCALATION_TERMINAL_DELETE_REVISION_RETRY"
        ),
    ))
}

fn delete_terminal_candidates(
    db: &Db,
    candidates: &[TerminalItemDeleteCandidate],
    context: &str,
) -> Result<usize, ErrorData> {
    let mut deleted = 0usize;
    for chunk in candidates.chunks(DELETE_BATCH_ROWS) {
        let chunk_deleted =
            delete_terminal_candidate_chunk(db, chunk, context, None)?.ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{context} non-cancellable delete returned cancellation"),
                )
            })?;
        deleted = deleted.saturating_add(chunk_deleted);
    }
    Ok(deleted)
}

async fn delete_terminal_candidates_cancellable(
    db: &Db,
    candidates: &[TerminalItemDeleteCandidate],
    context: &str,
    shutdown: &CancellationToken,
) -> Result<Option<usize>, ErrorData> {
    if shutdown.is_cancelled() {
        return Ok(None);
    }
    let mut deleted = 0usize;
    for (chunk_index, chunk) in candidates.chunks(DELETE_BATCH_ROWS).enumerate() {
        let Some(chunk_deleted) =
            delete_terminal_candidate_chunk(db, chunk, context, Some(shutdown))?
        else {
            tracing::info!(
                code = "ESCALATION_DELETE_BATCHES_CANCELLED",
                context,
                deleted_rows = deleted,
                pending_rows = candidates
                    .len()
                    .saturating_sub(chunk_index * DELETE_BATCH_ROWS),
                delete_batch_rows = DELETE_BATCH_ROWS,
                "stopping revision-guarded escalation row deletion between batches"
            );
            return Ok(None);
        };
        deleted = deleted.saturating_add(chunk_deleted);
        if (chunk_index + 1) * DELETE_BATCH_ROWS < candidates.len() {
            tokio::task::yield_now().await;
        }
    }
    Ok(Some(deleted))
}

fn terminal_item_delete_keys(
    now_unix_ms: u64,
    rows: &[EscalationItemRow],
) -> (Vec<TerminalItemDeleteCandidate>, usize) {
    let mut terminal = rows
        .iter()
        .filter(|row| !row.item.status.is_open())
        .map(|row| {
            (
                row.item.updated_at_unix_ms,
                TerminalItemDeleteCandidate {
                    key: row.key.clone(),
                    escalation_id: row.item.escalation_id.clone(),
                    scanned_updated_at_unix_ms: row.item.updated_at_unix_ms,
                },
            )
        })
        .collect::<Vec<_>>();
    terminal.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.key.cmp(&b.1.key)));

    let mut delete = terminal
        .iter()
        .filter(|(updated_at, _candidate)| {
            updated_at.saturating_add(TERMINAL_ITEM_RETENTION_MS) <= now_unix_ms
        })
        .map(|(_updated_at, candidate)| candidate.clone())
        .collect::<Vec<_>>();

    if terminal.len() > TERMINAL_ITEM_RETAIN_ROWS {
        let over_cap = terminal.len() - TERMINAL_ITEM_RETAIN_ROWS;
        delete.extend(
            terminal
                .iter()
                .take(over_cap)
                .map(|(_updated_at, candidate)| candidate.clone()),
        );
    }
    delete.sort_by(|left, right| left.key.cmp(&right.key));
    delete.dedup_by(|left, right| left.key == right.key);
    (delete, terminal.len())
}

fn log_terminal_item_prune(rows: &[EscalationItemRow], terminal_rows: usize, deleted: usize) {
    if deleted > 0 {
        tracing::info!(
            code = "ESCALATION_ITEM_RETENTION_PRUNED",
            scanned_rows = rows.len(),
            terminal_rows,
            deleted_rows = deleted,
            retain_terminal_rows = TERMINAL_ITEM_RETAIN_ROWS,
            retention_ms = TERMINAL_ITEM_RETENTION_MS,
            "readback=CF_KV escalation terminal item rows pruned"
        );
    } else if rows.len() > MAX_SCAN_ROWS {
        tracing::warn!(
            code = "ESCALATION_ITEM_QUEUE_LARGE",
            scanned_rows = rows.len(),
            terminal_rows,
            retain_terminal_rows = TERMINAL_ITEM_RETAIN_ROWS,
            "escalation item scan exceeded historical hard limit but continued"
        );
    }
}

fn prune_terminal_item_rows(
    db: &Db,
    now_unix_ms: u64,
    rows: &[EscalationItemRow],
) -> Result<usize, ErrorData> {
    let (delete, terminal_rows) = terminal_item_delete_keys(now_unix_ms, rows);
    let deleted = delete_terminal_candidates(db, &delete, "escalation terminal retention")?;
    log_terminal_item_prune(rows, terminal_rows, deleted);
    Ok(deleted)
}

async fn prune_terminal_item_rows_cancellable(
    db: &Db,
    now_unix_ms: u64,
    rows: &[EscalationItemRow],
    shutdown: &CancellationToken,
) -> Result<Option<usize>, ErrorData> {
    let (delete, terminal_rows) = terminal_item_delete_keys(now_unix_ms, rows);
    let Some(deleted) = delete_terminal_candidates_cancellable(
        db,
        &delete,
        "escalation terminal retention",
        shutdown,
    )
    .await?
    else {
        return Ok(None);
    };
    log_terminal_item_prune(rows, terminal_rows, deleted);
    Ok(Some(deleted))
}

fn prune_terminal_items(db: &Db, now_unix_ms: u64) -> Result<usize, ErrorData> {
    let rows = scan_item_rows(db)?;
    prune_terminal_item_rows(db, now_unix_ms, &rows)
}

/// All escalation items. Large queues are scanned in bounded storage windows;
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

#[derive(Clone, Debug)]
struct RevisionedEscalationPolicy {
    policy: EscalationPolicy,
    revision_sha256: Option<[u8; 32]>,
}

fn load_policy_revisioned(db: &Db) -> Result<RevisionedEscalationPolicy, ErrorData> {
    let key = CONFIG_KEY.as_bytes();
    let Some(revisioned) = db
        .get_cf_revisioned(cf::CF_KV, key)
        .map_err(storage_error)?
    else {
        return Ok(RevisionedEscalationPolicy {
            policy: EscalationPolicy::default(),
            revision_sha256: None,
        });
    };
    let value = live_revisioned_value(&revisioned, "escalation policy")?;
    let policy = decode_json::<EscalationPolicy>(value).map_err(|error| {
        mcp_error(
            error.code(),
            format!("escalation policy decode failed: {error}"),
        )
    })?;
    Ok(RevisionedEscalationPolicy {
        policy,
        revision_sha256: Some(revisioned.revision_sha256),
    })
}

fn load_policy(db: &Db) -> Result<EscalationPolicy, ErrorData> {
    load_policy_revisioned(db).map(|revisioned| revisioned.policy)
}

fn store_policy(
    db: &Db,
    policy: &EscalationPolicy,
    expected_revision_sha256: Option<[u8; 32]>,
) -> Result<EscalationPolicy, ErrorData> {
    let value = encode_json(policy).map_err(|error| {
        mcp_error(
            error.code(),
            format!("escalation policy encode failed: {error}"),
        )
    })?;
    let key = CONFIG_KEY.as_bytes().to_vec();
    let outcome = db
        .mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [RevisionGuard::new(key.clone(), expected_revision_sha256)],
            std::iter::empty::<Vec<u8>>(),
            [(key.clone(), value.clone())],
        )
        .map_err(storage_error)?;
    if !outcome.applied {
        let conflict = outcome.conflict.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                "escalation policy CAS reported applied=false without conflict detail",
            )
        })?;
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "escalation policy changed concurrently and was not overwritten: observed_seq={} expected_revision={} actual_revision={}; reread escalation_config_get and retry the intended update",
                outcome.committed_seq,
                expected_revision_sha256
                    .as_ref()
                    .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision)),
                conflict
                    .actual_revision_sha256
                    .as_ref()
                    .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision))
            ),
        ));
    }
    let committed_revision = outcome.committed_revisions_sha256[0].ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "escalation policy CAS committed without a row revision: committed_seq={}",
                outcome.committed_seq
            ),
        )
    })?;
    let readback = db
        .get_cf_revisioned(cf::CF_KV, &key)
        .map_err(storage_error)?
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "escalation policy row absent immediately after guarded write",
            )
        })?;
    let readback_value = live_revisioned_value(&readback, "escalation policy readback")?;
    if readback.revision_sha256 == committed_revision && readback_value != value.as_slice() {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            "escalation policy revision matched but bytes differed immediately after write",
        ));
    }
    let authoritative_policy =
        decode_json::<EscalationPolicy>(readback_value).map_err(|error| {
            mcp_error(
                error.code(),
                format!("authoritative escalation policy readback decode failed: {error}"),
            )
        })?;
    if readback.revision_sha256 != committed_revision {
        tracing::info!(
            code = "ESCALATION_POLICY_READBACK_SUPERSEDED",
            committed_seq = outcome.committed_seq,
            committed_revision = %hex_bytes(&committed_revision),
            latest_revision = %hex_bytes(&readback.revision_sha256),
            "separate policy readback decoded a newer valid policy revision; returning authoritative state"
        );
    }
    Ok(authoritative_policy)
}

fn approval_rows_for_opened_escalation(
    item: &EscalationItem,
    now_unix_ms: u64,
) -> Result<GuardedExtraRows, ErrorData> {
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
    let approval_key = approval_item_key(&item.approval_id);
    Ok(GuardedExtraRows {
        guards: vec![RevisionGuard::new(approval_key.clone(), None)],
        rows: vec![
            (approval_key, approval_item_value),
            (
                approval_audit_key(&item.approval_id, now_unix_ms, &approval_event_id),
                approval_audit_value,
            ),
        ],
    })
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
) -> Result<GuardedExtraRows, ErrorData> {
    let approval_key = approval_item_key(&item.approval_id);
    let Some(revisioned) = db
        .get_cf_revisioned(cf::CF_KV, &approval_key)
        .map_err(storage_error)?
    else {
        if item.approval_suppressed_reason.is_some() {
            return Ok(GuardedExtraRows::default());
        }
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "linked approval row is physically absent for escalation {}: approval_id={} key={}",
                item.escalation_id,
                item.approval_id,
                hex_bytes(&approval_key)
            ),
        ));
    };
    let approval_value = live_revisioned_value(
        &revisioned,
        &format!("linked approval {}", item.approval_id),
    )?;
    let mut approval = validate_linked_approval_item_identity(&approval_key, approval_value)?;
    validate_linked_approval_for_escalation(item, &approval)?;
    if approval_status_is_terminal(approval.status) {
        return Ok(GuardedExtraRows::default());
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
    Ok(GuardedExtraRows {
        guards: vec![RevisionGuard::new(
            approval_key.clone(),
            Some(revisioned.revision_sha256),
        )],
        rows: vec![(approval_key, approval_value), (audit_key, audit_value)],
    })
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
    for scanned_item in open_items_for_anchor(db, &transition.anchor)? {
        let mut resolved_or_superseded = false;
        for attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
            let Some(mut current) = read_item_revisioned(db, &scanned_item.escalation_id)? else {
                resolved_or_superseded = true;
                break;
            };
            if !current.item.status.is_open()
                || current.item.anchor != transition.anchor
                || current.item.attention_state == new_state
            {
                resolved_or_superseded = true;
                break;
            }
            let item = &mut current.item;
            item.status = EscalationStatus::Resolved;
            item.updated_at_unix_ms = now_unix_ms;
            item.next_escalate_at_unix_ms = None;
            item.closed_reason = Some(format!("state_change:{new_state}"));
            let mut approval_rows = linked_approval_terminal_rows(
                db,
                item,
                "linked_escalation_resolved",
                format!(
                    "linked escalation {} resolved by state_change:{new_state}",
                    item.escalation_id
                ),
            )?;
            approval_rows.extend(terminal_open_index_row(db, item)?);
            match write_item_and_audit_with_extra_rows_if_revision(
                db,
                item,
                "resolved",
                json!({ "reason": "state_change", "new_state": new_state }),
                approval_rows,
                current.revision_sha256,
            )? {
                ItemWriteOutcome::Applied { committed_seq, .. } => {
                    resolved_or_superseded = true;
                    superseded = true;
                    tracing::info!(
                        code = "ESCALATION_RESOLVED",
                        escalation_id = %item.escalation_id,
                        anchor = %item.anchor,
                        new_state,
                        committed_seq,
                        revision_attempt = attempt,
                        "readback=CF_KV escalation resolved by state change"
                    );
                    break;
                }
                ItemWriteOutcome::Conflict { observed_seq, .. } => {
                    tracing::info!(
                        code = "ESCALATION_TRANSITION_REVISION_RETRY",
                        escalation_id = %item.escalation_id,
                        anchor = %item.anchor,
                        new_state,
                        attempt,
                        max_attempts = ACK_REVISION_MAX_ATTEMPTS,
                        observed_seq,
                        "state-transition resolution lost a revision race; rereading authoritative escalation and linked approval"
                    );
                }
            }
        }
        if !resolved_or_superseded {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "escalation {} remained open for obsolete state after {ACK_REVISION_MAX_ATTEMPTS} revision attempts; inspect ESCALATION_TRANSITION_REVISION_RETRY and competing writers",
                    scanned_item.escalation_id
                ),
            ));
        }
    }

    // 2. Open a new escalation when the new state is attention-worthy and no
    //    open escalation already exists for this exact (anchor, state).
    let Some(severity) = severity_for(transition.state_to) else {
        if superseded {
            wake_worker();
        }
        return Ok(());
    };
    if let Some(policy_suppressed_reason) = operator_interrupt_suppressed_reason(transition) {
        tracing::info!(
            code = "ESCALATION_SUPPRESSED",
            anchor = %transition.anchor,
            state_to = transition.state_to.as_str(),
            reason_code = %transition.reason_code,
            policy_suppressed_reason = %policy_suppressed_reason,
            "operator-facing escalation suppressed by policy before item creation"
        );
        if superseded {
            wake_worker();
        }
        return Ok(());
    }
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
    let prior_index = read_open_index(db, &transition.anchor, transition.state_to.as_str())?;
    if let Some(index) = &prior_index
        && index.record.is_open
    {
        return indexed_open_item(db, index);
    }
    let expected_index_revision = prior_index.as_ref().map(|index| index.revision_sha256);
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
    let mut extra_rows = if item.approval_suppressed_reason.is_some() {
        GuardedExtraRows::default()
    } else {
        approval_rows_for_opened_escalation(&item, now_unix_ms)?
    };
    let approval_row_written = !extra_rows.is_empty();
    extra_rows.extend(open_index_row(&item, true, expected_index_revision)?);
    let create_outcome = create_item_and_audit_with_extra_rows(
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
            "approval_row_written": approval_row_written,
        }),
        extra_rows,
    )?;
    if let ItemWriteOutcome::Conflict {
        observed_seq,
        actual_revision_sha256,
    } = create_outcome
    {
        let winner = read_open_index(
            db,
            &transition.anchor,
            transition.state_to.as_str(),
        )?
        .filter(|index| index.record.is_open)
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "escalation create conflicted but no authoritative open-index winner exists: candidate_escalation_id={} observed_seq={observed_seq} actual_revision={}",
                    item.escalation_id,
                    actual_revision_sha256
                        .as_ref()
                        .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision))
                ),
            )
        })?;
        let winner = indexed_open_item(db, &winner)?;
        tracing::info!(
            code = "ESCALATION_OPEN_RACE_COALESCED",
            candidate_escalation_id = %item.escalation_id,
            winner_escalation_id = %winner.escalation_id,
            anchor = %winner.anchor,
            attention_state = %winner.attention_state,
            observed_seq,
            "deterministic open-index CAS prevented a duplicate escalation"
        );
        return Ok(winner);
    }
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

#[derive(Clone, Debug)]
struct AckEscalationOutcome {
    escalation: EscalationItem,
    newly_acked: bool,
}

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
) -> Result<AckEscalationOutcome, ErrorData> {
    for attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let mut current = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("escalation {escalation_id} not found"),
            )
        })?;
        if current.item.status != EscalationStatus::Pending {
            // Already acked/resolved/expired — honest idempotent report.
            return Ok(AckEscalationOutcome {
                escalation: current.item,
                newly_acked: false,
            });
        }
        current.item.status = EscalationStatus::Acked;
        current.item.updated_at_unix_ms = now_unix_ms;
        current.item.acked_at_unix_ms = Some(now_unix_ms);
        current.item.acked_via = Some(via.to_owned());
        current.item.next_escalate_at_unix_ms = None;
        match write_item_and_audit_if_revision(
            db,
            &current.item,
            "acked",
            json!({ "via": via, "note": note }),
            current.revision_sha256,
        )? {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback = read_item(db, escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "acknowledged escalation {escalation_id} disappeared after committed_seq={committed_seq}"
                        ),
                    )
                })?;
                if readback.status == EscalationStatus::Pending {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "acknowledged escalation {escalation_id} read back pending after committed_seq={committed_seq}"
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_ACKED",
                    escalation_id,
                    via,
                    committed_seq,
                    ack_revision_attempt = attempt,
                    readback_status = readback.status.as_str(),
                    "readback=CF_KV escalation acknowledged; ladder stopped"
                );
                return Ok(AckEscalationOutcome {
                    escalation: readback,
                    newly_acked: true,
                });
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_ACK_REVISION_RETRY",
                    escalation_id,
                    via,
                    attempt,
                    max_attempts = ACK_REVISION_MAX_ATTEMPTS,
                    observed_seq,
                    "acknowledgment lost an optimistic revision race; rereading authoritative item state"
                );
            }
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "escalation {escalation_id} acknowledgment could not acquire a stable revision after {ACK_REVISION_MAX_ATTEMPTS} attempts; inspect ESCALATION_ACK_REVISION_RETRY and competing item writers"
        ),
    ))
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
    let linked_item = read_item(db, escalation_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "agent escalation approval {} points to missing escalation {}",
                approval.approval_id, escalation_id
            ),
        )
    })?;
    validate_linked_approval_for_escalation(&linked_item, approval)?;
    let via = format!("approval_decide:{decision}");
    let outcome = ack_escalation(db, escalation_id, &via, note, now_unix_ms)?;
    tracing::info!(
        code = "ESCALATION_ACKED_FROM_APPROVAL",
        escalation_id,
        approval_id = %approval.approval_id,
        decision,
        by_session,
        newly_acked = outcome.newly_acked,
        "approval decision acknowledged escalation and stopped the ladder"
    );
    Ok(Some(outcome.escalation))
}

// ---------------------------------------------------------------------------
// Worker: Tier 0 toast + Tier 1 webhook ladder + TTL expiry (async)
// ---------------------------------------------------------------------------

/// Outcome of one [`process_pending`] sweep for structured worker readback.
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
    shutdown: &CancellationToken,
) -> Result<Option<ProcessReport>, ErrorData> {
    if shutdown.is_cancelled() {
        return Ok(None);
    }
    let sweep_started = Instant::now();
    let mut report = ProcessReport::default();
    let Some(scan) = scan_item_rows_cancellable(db, shutdown).await? else {
        tracing::info!(
            code = "ESCALATION_WORKER_STOPPED",
            stage = "during_item_scan",
            elapsed_ms = sweep_started.elapsed().as_millis(),
            "stopping escalation worker at candidate-page cancellation checkpoint"
        );
        return Ok(None);
    };
    if scan.elapsed_ms >= WORKER_SLOW_SCAN_LOG_MS {
        tracing::warn!(
            code = "ESCALATION_ITEM_SCAN_SLOW",
            scan_elapsed_ms = scan.elapsed_ms,
            scan_rows = scan.rows.len(),
            scan_pages = scan.pages,
            candidate_rows_examined = scan.candidate_rows_examined,
            expired_rows_skipped = scan.expired_rows_skipped,
            first_snapshot_seq = scan.first_snapshot_seq,
            last_snapshot_seq = scan.last_snapshot_seq,
            snapshot_seq_changes = scan.snapshot_seq_changes,
            requested_candidate_rows = SCAN_CHUNK_ROWS,
            item_key_len = ITEM_KEY_LEN,
            "candidate-bounded escalation item scan exceeded the latency budget"
        );
    }
    let rows = scan.rows;
    if shutdown.is_cancelled() {
        tracing::info!(
            code = "ESCALATION_WORKER_STOPPED",
            stage = "after_item_scan",
            scan_rows = rows.len(),
            elapsed_ms = sweep_started.elapsed().as_millis(),
            "stopping escalation worker at cooperative cancellation checkpoint"
        );
        return Ok(None);
    }
    let Some(pruned) =
        prune_terminal_item_rows_cancellable(db, now_unix_ms, &rows, shutdown).await?
    else {
        tracing::info!(
            code = "ESCALATION_WORKER_STOPPED",
            stage = "during_terminal_prune",
            scan_rows = rows.len(),
            elapsed_ms = sweep_started.elapsed().as_millis(),
            "stopping escalation worker at terminal-prune batch cancellation checkpoint"
        );
        return Ok(None);
    };
    if shutdown.is_cancelled() {
        tracing::info!(
            code = "ESCALATION_WORKER_STOPPED",
            stage = "after_terminal_prune",
            scan_rows = rows.len(),
            pruned_rows = pruned,
            elapsed_ms = sweep_started.elapsed().as_millis(),
            "stopping escalation worker at cooperative cancellation checkpoint"
        );
        return Ok(None);
    }
    let items: Vec<EscalationItem> = if pruned > 0 {
        let Some(rescan) = scan_item_rows_cancellable(db, shutdown).await? else {
            tracing::info!(
                code = "ESCALATION_WORKER_STOPPED",
                stage = "during_post_prune_rescan",
                pruned_rows = pruned,
                elapsed_ms = sweep_started.elapsed().as_millis(),
                "stopping escalation worker at post-prune page cancellation checkpoint"
            );
            return Ok(None);
        };
        rescan.rows.into_iter().map(|row| row.item).collect()
    } else {
        rows.into_iter().map(|row| row.item).collect()
    };
    if shutdown.is_cancelled() {
        tracing::info!(
            code = "ESCALATION_WORKER_STOPPED",
            stage = "after_item_materialization",
            scan_rows = items.len(),
            elapsed_ms = sweep_started.elapsed().as_millis(),
            "stopping escalation worker at cooperative cancellation checkpoint"
        );
        return Ok(None);
    }
    let authoritative_agent_reads = super::agent_state::reads(now_unix_ms);
    report.scanned = items.len();
    for (item_index, scanned_item) in items.into_iter().enumerate() {
        if item_index % WORKER_COOPERATIVE_YIELD_ROWS == 0 {
            tokio::task::yield_now().await;
        }
        if shutdown.is_cancelled() {
            tracing::info!(
                code = "ESCALATION_WORKER_STOPPED",
                stage = "before_item",
                item_index,
                scan_rows = report.scanned,
                elapsed_ms = sweep_started.elapsed().as_millis(),
                "stopping escalation worker at cooperative cancellation checkpoint"
            );
            return Ok(None);
        }
        let Some(current) = read_item_revisioned(db, &scanned_item.escalation_id)? else {
            tracing::debug!(
                code = "ESCALATION_WORKER_ITEM_DISAPPEARED",
                escalation_id = %scanned_item.escalation_id,
                item_index,
                "scanned escalation item was removed before its point-read processing boundary"
            );
            continue;
        };
        let RevisionedEscalationItem {
            mut item,
            revision_sha256: mut item_revision_sha256,
        } = current;
        if !item.status.is_open() {
            if remove_tier0_if_terminal(db, &mut item, &mut item_revision_sha256).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            if matches!(
                item.status,
                EscalationStatus::Resolved | EscalationStatus::Expired
            ) {
                let mut terminal_rows = linked_approval_terminal_rows(
                    db,
                    &item,
                    "linked_escalation_already_closed",
                    format!(
                        "linked escalation {} is already {}",
                        item.escalation_id,
                        item.status.as_str()
                    ),
                )?;
                let approval_closed = !terminal_rows.is_empty();
                terminal_rows.extend(terminal_open_index_row(db, &item)?);
                if !terminal_rows.is_empty() {
                    item.updated_at_unix_ms = now_unix_ms;
                    let outcome = write_item_and_audit_with_extra_rows_if_revision(
                        db,
                        &item,
                        "linked_approval_closed",
                        json!({
                            "reason": "linked_escalation_already_closed",
                            "status": item.status.as_str(),
                            "approval_id": &item.approval_id,
                        }),
                        terminal_rows,
                        item_revision_sha256,
                    )?;
                    if accept_applied_item_revision(outcome, &mut item_revision_sha256).is_none() {
                        continue;
                    }
                    if approval_closed {
                        report.linked_approvals_closed += 1;
                    }
                }
            }
            continue;
        }
        if let Some(agent_state) =
            authoritative_agent_read_for_item(&authoritative_agent_reads, &item)
            && (agent_state.state == AgentLifecycleState::Dead
                || agent_state.state.as_str() != item.attention_state)
        {
            item.status = EscalationStatus::Resolved;
            item.updated_at_unix_ms = now_unix_ms;
            item.next_escalate_at_unix_ms = None;
            let authoritative_state = agent_state.state.as_str();
            let reason = agent_state
                .reason_code
                .as_deref()
                .unwrap_or("state_changed");
            item.closed_reason = Some(format!(
                "authoritative_agent_state:{authoritative_state}:{reason}"
            ));
            let mut approval_rows = linked_approval_terminal_rows(
                db,
                &item,
                "linked_escalation_resolved",
                format!(
                    "linked escalation {} resolved because authoritative agent state is {authoritative_state}:{reason}",
                    item.escalation_id,
                ),
            )?;
            let approval_closed = !approval_rows.is_empty();
            approval_rows.extend(terminal_open_index_row(db, &item)?);
            let outcome = write_item_and_audit_with_extra_rows_if_revision(
                db,
                &item,
                "resolved",
                json!({
                    "reason": "authoritative_agent_state",
                    "agent_state": agent_state,
                }),
                approval_rows,
                item_revision_sha256,
            )?;
            let Some(committed_seq) =
                accept_applied_item_revision(outcome, &mut item_revision_sha256)
            else {
                continue;
            };
            if approval_closed {
                report.linked_approvals_closed += 1;
            }
            report.terminal_resolved += 1;
            tracing::info!(
                code = "ESCALATION_RESOLVED",
                escalation_id = %item.escalation_id,
                anchor = %item.anchor,
                authoritative_state,
                reason,
                committed_seq,
                "readback=CF_KV escalation resolved because its attention state is no longer authoritative"
            );
            if remove_tier0_if_terminal(db, &mut item, &mut item_revision_sha256).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            continue;
        }
        if item.status == EscalationStatus::Acked {
            if remove_tier0_if_terminal(db, &mut item, &mut item_revision_sha256).await? {
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
            let mut approval_rows = linked_approval_terminal_rows(
                db,
                &item,
                "linked_escalation_expired",
                format!("linked escalation {} expired by ttl", item.escalation_id),
            )?;
            let approval_closed = !approval_rows.is_empty();
            approval_rows.extend(terminal_open_index_row(db, &item)?);
            let outcome = write_item_and_audit_with_extra_rows_if_revision(
                db,
                &item,
                "expired",
                json!({ "ttl_ms": item.expires_at_unix_ms }),
                approval_rows,
                item_revision_sha256,
            )?;
            let Some(committed_seq) =
                accept_applied_item_revision(outcome, &mut item_revision_sha256)
            else {
                continue;
            };
            if approval_closed {
                report.linked_approvals_closed += 1;
            }
            report.expired += 1;
            tracing::warn!(
                code = "ESCALATION_EXPIRED",
                escalation_id = %item.escalation_id,
                committed_seq,
                "readback=CF_KV escalation expired with no acknowledgment"
            );
            if remove_tier0_if_terminal(db, &mut item, &mut item_revision_sha256).await? {
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
                    let outcome = write_item_and_audit_if_revision(
                        db,
                        &item,
                        "tier0_toast_fired",
                        json!({ "suppress_popup": item.tier0_quiet_digest }),
                        item_revision_sha256,
                    )?;
                    if accept_applied_item_revision(outcome, &mut item_revision_sha256).is_none() {
                        tracing::warn!(
                            code = "ESCALATION_TIER0_RESULT_REVISION_CONFLICT",
                            escalation_id = %item.escalation_id,
                            "toast completed after authoritative item state changed; stale state was not written and the next sweep will reconcile removal"
                        );
                        continue;
                    }
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
            let Some(latest) = read_item_revisioned(db, &item.escalation_id)? else {
                continue;
            };
            item = latest.item;
            item_revision_sha256 = latest.revision_sha256;
            if item.status != EscalationStatus::Pending
                || !item.tier1_eligible
                || item
                    .next_escalate_at_unix_ms
                    .is_none_or(|due_at| now_unix_ms < due_at)
            {
                continue;
            }
            let policy = load_policy(db)?;
            let index = item.ladder_index as usize;
            if let Some(channel) = policy.webhooks.get(index).cloned() {
                let ladder_index = item.ladder_index;
                let attempt_number = next_channel_attempt_number(&item, ladder_index);
                let attempt =
                    deliver_webhook(&channel, &item, ladder_index, attempt_number, now_unix_ms)
                        .await;
                let attempt_ok = attempt.ok;
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
                let outcome = write_item_and_audit_if_revision(
                    db,
                    &item,
                    "tier1_channel_attempt",
                    event_detail,
                    item_revision_sha256,
                )?;
                if accept_applied_item_revision(outcome, &mut item_revision_sha256).is_none() {
                    tracing::error!(
                        code = "ESCALATION_WEBHOOK_RESULT_REVISION_CONFLICT",
                        escalation_id = %item.escalation_id,
                        ladder_index,
                        attempt_number,
                        issue = 1757,
                        "webhook returned after authoritative item state changed; stale state was not written, but the remote outcome requires durable-outbox reconciliation"
                    );
                    continue;
                }
                if attempt_ok {
                    report.tier1_fired += 1;
                } else {
                    report.tier1_failed += 1;
                }
                dirty = false;
            } else {
                // No channel at this index (config shrank): stop the ladder.
                item.next_escalate_at_unix_ms = None;
                dirty = true;
            }
        }

        if dirty {
            item.updated_at_unix_ms = now_unix_ms;
            let outcome = write_item_and_audit_if_revision(
                db,
                &item,
                "updated",
                json!({}),
                item_revision_sha256,
            )?;
            let _ = accept_applied_item_revision(outcome, &mut item_revision_sha256);
        }
    }
    Ok(Some(report))
}

async fn remove_tier0_if_terminal(
    db: &Db,
    item: &mut EscalationItem,
    item_revision_sha256: &mut [u8; 32],
) -> Result<bool, ErrorData> {
    if !item.tier0_fired || item.tier0_toast_removed.is_some() {
        return Ok(false);
    }
    let tag = escalation_toast_tag(&item.escalation_id);
    let outcome = remove_internal_toast(tag).await;
    let removed =
        outcome.removed || outcome.already_absent || outcome.status == "unsupported_platform";
    item.tier0_toast_removed = Some(outcome.clone());
    let write_outcome = write_item_and_audit_if_revision(
        db,
        item,
        "tier0_toast_removed",
        json!({
            "toast_removal": &outcome,
        }),
        *item_revision_sha256,
    )?;
    if accept_applied_item_revision(write_outcome, item_revision_sha256).is_none() {
        tracing::warn!(
            code = "ESCALATION_TIER0_REMOVAL_REVISION_CONFLICT",
            escalation_id = %item.escalation_id,
            "Action Center removal completed after the authoritative item changed; stale item state was not written"
        );
        if let Some(latest) = read_item_revisioned(db, &item.escalation_id)? {
            *item = latest.item;
            *item_revision_sha256 = latest.revision_sha256;
        }
        return Ok(false);
    }
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

fn authoritative_agent_read_for_item(
    authoritative_agent_reads: &[AgentStateRead],
    item: &EscalationItem,
) -> Option<AgentStateRead> {
    authoritative_agent_reads
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
                    tracing::info!(code = "ESCALATION_WORKER_STOPPED", stage = "idle_wait", "stopping escalation worker");
                    break;
                }
                _ = signal.notified() => {}
                _ = interval.tick() => {}
            }
            if shutdown.is_cancelled() {
                tracing::info!(
                    code = "ESCALATION_WORKER_STOPPED",
                    stage = "before_sweep",
                    "stopping escalation worker before starting a sweep"
                );
                break;
            }
            let now_unix_ms = unix_time_ms_now();
            let sweep_result = tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!(
                        code = "ESCALATION_WORKER_STOPPED",
                        stage = "during_pending_sweep",
                        "stopping escalation worker during pending sweep"
                    );
                    break;
                }
                result = process_pending(&db, now_unix_ms, &shutdown) => result,
            };
            match sweep_result {
                Ok(Some(report))
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
                Ok(Some(_quiet)) => {}
                Ok(None) => {
                    tracing::info!(
                        code = "ESCALATION_WORKER_STOPPED",
                        stage = "cooperative_checkpoint",
                        "stopping escalation worker after cooperative cancellation checkpoint"
                    );
                    break;
                }
                Err(error) => {
                    tracing::error!(
                        code = "ESCALATION_SWEEP_FAILED",
                        detail = %error.message,
                        "escalation sweep failed; will retry next tick"
                    );
                }
            }
            if shutdown.is_cancelled() {
                tracing::info!(
                    code = "ESCALATION_WORKER_STOPPED",
                    stage = "before_orphan_cleanup",
                    "stopping escalation worker before orphan toast cleanup"
                );
                break;
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
                let report = tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!(
                            code = "ESCALATION_WORKER_STOPPED",
                            stage = "during_orphan_cleanup",
                            "stopping escalation worker during orphan toast cleanup"
                        );
                        break;
                    }
                    report = remove_orphaned_escalation_toasts(preserve_tags) => report,
                };
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
        let revisioned_policy = load_policy_revisioned(&db)?;
        let mut policy = revisioned_policy.policy;
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
        let authoritative_policy = store_policy(&db, &policy, revisioned_policy.revision_sha256)?;
        Ok(Json(EscalationConfigResponse::from_policy(
            &authoritative_policy,
        )))
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
        let outcome = ack_escalation(
            &db,
            &params.escalation_id,
            "escalation_ack_tool",
            params.note.as_deref(),
            unix_time_ms_now(),
        )?;
        Ok(Json(EscalationAckResponse {
            escalation: outcome.escalation,
            newly_acked: outcome.newly_acked,
        }))
    }
}

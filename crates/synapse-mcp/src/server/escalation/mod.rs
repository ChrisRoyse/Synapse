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
//!   threshold, POST a structured packet to the operator's idempotency-aware
//!   receiver or gateway. The receiver must prove the `synapse_receipt_v1`
//!   contract before POST; a gateway may then fan out to self-hosted ntfy,
//!   Pushover, Telegram/Discord, or a phone-call service under its own durable
//!   deduplication boundary. Synapse ships **no** commercial push SaaS and
//!   requires none. With no egress configured the engine makes **zero**
//!   outbound network calls.
//!
//! Truth lives in `CF_KV`, never in daemon memory (the durable approval-queue
//! pattern, #867):
//! - `escalation/v1/config` — the operator policy (webhooks, threshold, quiet
//!   hours, ack window). A single row; absent row ⇒ Tier-0-only defaults.
//! - `escalation/v1/item/{escalation_id}` — current escalation state.
//! - `escalation/v1/outbox/{escalation_id}/{ladder_index:08}` — the durable
//!   webhook delivery intent and outcome. The in-flight row is committed before
//!   any network I/O and carries the stable receiver idempotency key.
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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    MAX_BODY_CHARS, MAX_TITLE_CHARS, NotifyHumanParams, NotifyHumanResponse, NotifyKind,
    PreparedToastPayload, SYNAPSE_AUMID, SYNAPSE_ESCALATION_TOAST_GROUP, SYNAPSE_TOAST_GROUP,
    TOAST_RENDERER_VERSION_CURRENT, TOAST_RENDERER_VERSION_V1, ToastCleanupReport,
    ToastHistoryReadback, ToastPreShowAuthorizer, ToastPreShowFailure, ToastRemovalOutcome,
    ToastShowAuthority, WINDOWS_ACTION_CENTER_MAX_HISTORY_MS, inspect_internal_escalation_toast,
    inspect_internal_toast, legacy_prepared_toast_payload_v1_valid, platform_corrected_expiration,
    prepare_internal_escalation_toast, prepared_toast_payload_matches_request,
    prepared_toast_payload_valid, remove_internal_escalation_toast, remove_internal_toast,
    remove_orphaned_escalation_toasts, run_internal_escalation_toast_blocking, toast_tag_for,
    toast_text_char_allowed, upgrade_prepared_toast_payload_v1,
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
        self.rows.is_empty() && self.guards.is_empty()
    }

    fn extend(&mut self, other: Self) {
        self.rows.extend(other.rows);
        self.guards.extend(other.guards);
    }
}

const CONFIG_KEY: &str = "escalation/v1/config";
const ITEM_PREFIX: &str = "escalation/v1/item/";
const AUDIT_PREFIX: &str = "escalation/v1/audit/";
const OUTBOX_PREFIX: &str = "escalation/v1/outbox/";
const OPEN_INDEX_PREFIX: &str = "escalation/v1/open/";
const RECENT_INDEX_PREFIX: &str = "escalation/v2/recent/";
const RECENT_INDEX_MIGRATION_KEY: &str = "escalation/v2/recent_migration/v1";
const PROJECTION_WATERMARK_PREFIX: &str = "escalation/v2/projection/";
const PENDING_PROJECTION_INDEX_PREFIX: &str = "escalation/v2/projection_pending/";
const PENDING_PROJECTION_INDEX_MIGRATION_KEY: &str =
    "escalation/v2/projection_pending_migration/v1";
const PROJECTION_AUDIT_PREFIX: &str = "escalation/v2/projection_audit/";
const PROJECTION_CURSOR_VERSION: u32 = 2;
const PENDING_PROJECTION_INDEX_VERSION: u32 = 1;
const PROJECTION_ANCHOR_DIGEST_HEX_LEN: usize = 64;
const PROJECTION_WATERMARK_KEY_LEN: usize =
    PROJECTION_WATERMARK_PREFIX.len() + PROJECTION_ANCHOR_DIGEST_HEX_LEN;
const PENDING_PROJECTION_INDEX_KEY_LEN: usize =
    PENDING_PROJECTION_INDEX_PREFIX.len() + PROJECTION_ANCHOR_DIGEST_HEX_LEN;
/// Pending projection work can perform multiple guarded item/approval writes
/// per row. Keep pages deliberately small so the worker releases the
/// transition lock and observes cooperative cancellation at a tight bound.
const PENDING_PROJECTION_PAGE_ROWS: usize = 16;
const ORPHAN_TOAST_AUDIT_PREFIX: &str = "escalation/v1/toast_orphan_cleanup/";
const ESCALATION_ID_PREFIX: &str = "esc1-";
const ESCALATION_ID_HEX_LEN: usize = 32;
const ESCALATION_ID_LEN: usize = ESCALATION_ID_PREFIX.len() + ESCALATION_ID_HEX_LEN;
const ITEM_KEY_LEN: usize = ITEM_PREFIX.len() + ESCALATION_ID_LEN;
const RECENT_INDEX_KEY_LEN: usize = RECENT_INDEX_PREFIX.len() + 16 + 1 + ESCALATION_ID_LEN;
const OPEN_INDEX_KEY_LEN: usize = OPEN_INDEX_PREFIX.len() + 64;
const LIST_INDEX_PAGE_ROWS: usize = 64;
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
const MIN_SECRET_CHARS: usize = 32;
const WEBHOOK_TIMEOUT_MS: u64 = 15_000;
const WEBHOOK_IDEMPOTENCY_PROTOCOL_HEADER: &str = "X-Synapse-Idempotency-Protocol";
const WEBHOOK_DELIVERY_ID_HEADER: &str = "X-Synapse-Delivery-Id";
const WEBHOOK_IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
const WEBHOOK_BODY_SHA256_HEADER: &str = "X-Synapse-Body-SHA256";
const WEBHOOK_RECEIPT_STATE_HEADER: &str = "X-Synapse-Receipt-State";
const WEBHOOK_SIGNATURE_HEADER: &str = "X-Synapse-Signature";
const WEBHOOK_SIGNATURE_TIMESTAMP_HEADER: &str = "X-Synapse-Signature-Timestamp-Ms";
const WEBHOOK_SIGNATURE_AUDIENCE_HEADER: &str = "X-Synapse-Signature-Audience";
const WEBHOOK_SIGNATURE_MAX_AGE_HEADER: &str = "X-Synapse-Signature-Max-Age-Ms";
const WEBHOOK_SIGNATURE_MAX_AGE_MS: u64 = 5 * 60 * 1_000;
const WEBHOOK_IDEMPOTENCY_PROTOCOL_V1: &str = "synapse_echo_v1";
const WEBHOOK_RECEIPT_PROTOCOL_V1: &str = "synapse_receipt_v1";
const WEBHOOK_RECEIPT_READY: &str = "durable_idempotency_ready";
const WEBHOOK_RECEIPT_COMMITTED: &str = "committed";
const WEBHOOK_RECEIPT_NOT_COMMITTED: &str = "not_committed";
const RECEIVER_GENERATION_PREFIX: &str = "rcv1-";
const DAEMON_EPOCH_PREFIX: &str = "dme1-";
const WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL: u32 = 3;
const WEBHOOK_RETRY_BASE_BACKOFF_MS: u64 = 30_000;
const WEBHOOK_RETRY_MAX_BACKOFF_MS: u64 = DEFAULT_ACK_WINDOW_MS;
const WORKER_TICK_MS: u64 = 1_000;
const TIER0_REMOVAL_RETRY_BASE_MS: u64 = 60_000;
const TIER0_REMOVAL_RETRY_MAX_MS: u64 = 60 * 60 * 1_000;
const ACK_REVISION_MAX_ATTEMPTS: usize = 16;
const DELETE_REVISION_MAX_ATTEMPTS: usize = 16;
const AMBIENT_SILENT_TIMEOUT_SUPPRESSED: &str = "ambient_unprobeable_silent_timeout";

fn checked_unix_time_ms(context: &str) -> Result<u64, ErrorData> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "system clock precedes Unix epoch at {context}; refusing external side-effect authorization: {error}"
            ),
        )
    })?;
    u64::try_from(duration.as_millis()).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "system time does not fit u64 milliseconds at {context}; refusing external side-effect authorization: {error}"
            ),
        )
    })
}

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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WebhookIdempotencyContract {
    /// Historical configuration created before receiver-side deduplication was
    /// mandatory. It remains visible for migration but is rejected on every
    /// new config write and never permits network I/O.
    #[default]
    UnsupportedLegacy,
    /// Before POST, Synapse sends an OPTIONS probe carrying
    /// `X-Synapse-Idempotency-Protocol: synapse_echo_v1` and the stable
    /// `X-Synapse-Delivery-Id`. The endpoint must echo both headers on the
    /// OPTIONS response and every POST response. The POST also carries the
    /// delivery ID as the standard `Idempotency-Key` header.
    SynapseEchoV1,
    /// Receiver contract with an explicit durable boundary. OPTIONS must prove
    /// `durable_idempotency_ready`. POST responses must bind the delivery ID and
    /// body digest and declare either `committed` (2xx) or `not_committed`
    /// (non-2xx). A missing or contradictory receipt is an unknown terminal
    /// outcome, never a retryable failure.
    SynapseReceiptV1,
}

impl WebhookIdempotencyContract {
    const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedLegacy => "unsupported_legacy",
            Self::SynapseEchoV1 => WEBHOOK_IDEMPOTENCY_PROTOCOL_V1,
            Self::SynapseReceiptV1 => WEBHOOK_RECEIPT_PROTOCOL_V1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebhookChannel {
    /// Stable operator-controlled identity. Reordering the ladder must never
    /// create a new delivery identity for the same logical receiver.
    #[serde(default)]
    pub channel_id: String,
    /// Operator label for the channel (shown in the ladder audit).
    pub name: String,
    /// Target URL the structured packet is POSTed to.
    pub url: String,
    /// Required receiver-side deduplication protocol. Synapse refuses to POST
    /// unless the endpoint proves the durable receipt contract and binds the
    /// durable delivery identity and body digest; generic at-least-once
    /// webhooks and the historical echo-only contract are unsupported for new
    /// writes.
    #[serde(default)]
    pub idempotency_contract: WebhookIdempotencyContract,
    /// Optional shared secret. When set, the request carries a v2 HMAC-SHA256
    /// signature over a length-framed envelope containing POST, the canonical
    /// target URI, protocol, delivery ID, timestamp, body digest, and exact JSON
    /// bytes. The receiver must enforce the timestamp replay window and compare
    /// the digest in constant time.
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
    /// Opaque non-secret generation for each stable channel ID. It changes only
    /// when that receiver's URL, name, contract, or secret bytes change, so
    /// unrelated policy edits cannot invalidate a durable delivery intent.
    #[serde(default)]
    pub receiver_generations: BTreeMap<String, String>,
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
            receiver_generations: BTreeMap::new(),
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChannelAttempt {
    /// Stable logical-delivery identifier reused across retries.
    /// Empty only on historical rows written before the durable-outbox
    /// protocol; those rows are paired with `legacy_unclassified` and are
    /// never treated as proof of idempotent delivery.
    #[serde(default)]
    pub delivery_id: String,
    #[serde(default)]
    pub channel_id: String,
    pub channel_name: String,
    pub url_host: String,
    #[serde(default)]
    pub ladder_index: u32,
    #[serde(default)]
    pub attempt_number: u32,
    #[serde(default)]
    pub outcome: WebhookAttemptOutcome,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub signed: bool,
    pub at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WebhookAttemptOutcome {
    /// Historical attempt written before outcome provenance was durable. It is
    /// displayed honestly and never authorizes a retry.
    #[default]
    LegacyUnclassified,
    Accepted,
    /// The endpoint returned a protocol-valid non-success response. Retrying
    /// the same durable delivery ID is safe under the verified contract.
    TransientFailure,
    /// The request may have reached the endpoint, but no conclusive response
    /// was observed. This is never represented as "unsent".
    Unknown,
    /// Receiver contract was proven, but the escalation stopped before Synapse
    /// atomically claimed permission to begin POST.
    Abandoned,
    /// No remote side effect occurred (for example, preflight rejected the
    /// protocol) or local configuration drifted before POST. Synapse fails
    /// closed and does not retry it.
    TerminalFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebhookOutboxState {
    /// Durable intent exists; contract preflight may run, but POST has not been
    /// claimed and therefore cannot have occurred.
    InFlight,
    /// Item and outbox CAS proved the escalation was still pending immediately
    /// before POST. A crash from this state is an explicit unknown outcome.
    PostStarted,
    Accepted,
    TransientFailure,
    Unknown,
    /// A remote side effect remains possible, but retry is unsafe because the
    /// receiver stopped proving the contract or its configuration disappeared.
    UnknownTerminal,
    RetryExhausted,
    Abandoned,
    TerminalFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebhookReceiptState {
    Committed,
    NotCommitted,
}

impl WebhookReceiptState {
    const fn as_header(self) -> &'static str {
        match self {
            Self::Committed => WEBHOOK_RECEIPT_COMMITTED,
            Self::NotCommitted => WEBHOOK_RECEIPT_NOT_COMMITTED,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookOutboxRecord {
    schema_version: u32,
    delivery_id: String,
    escalation_id: String,
    ladder_index: u32,
    channel_id: String,
    channel_name: String,
    url_host: String,
    idempotency_contract: WebhookIdempotencyContract,
    /// SHA-256 over the stable channel ID, label, public origin, contract, and
    /// whether a secret is configured. URL path and secret bytes are
    /// deliberately excluded so this row cannot act as an offline credential
    /// verifier. Exact receiver bytes are authorized by `receiver_generation`
    /// plus a guarded policy read before any POST can leave the host.
    channel_fingerprint_sha256: String,
    /// Opaque receiver generation selected by the guarded policy. This value is
    /// random and non-secret; unlike a content revision, it cannot be used to
    /// verify guesses of the configured secret offline.
    receiver_generation: String,
    /// Exact bytes (JSON is UTF-8) and digest frozen before the first send.
    body_json: String,
    body_sha256: String,
    state: WebhookOutboxState,
    attempt_number: u32,
    attempt_started_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    contract_verified: bool,
    signed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_delivery_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_body_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_receipt_state: Option<WebhookReceiptState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_started_owner_epoch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookPayload {
    schema: String,
    channel_id: String,
    channel: String,
    escalation_id: String,
    severity: String,
    attention_state: String,
    anchor: String,
    spawn_id: Option<String>,
    session_id: Option<String>,
    reason_code: Option<String>,
    ladder_index: u32,
    created_at_unix_ms: u64,
    context: EscalationContext,
}

#[derive(Clone, Debug)]
struct RevisionedWebhookOutbox {
    record: WebhookOutboxRecord,
    revision_sha256: [u8; 32],
}

#[derive(Clone, Debug)]
struct WebhookDeliveryResult {
    attempt: ChannelAttempt,
    state: WebhookOutboxState,
    contract_verified: bool,
    response_delivery_id: Option<String>,
    response_body_sha256: Option<String>,
    response_receipt_state: Option<WebhookReceiptState>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Tier0ToastDelivery {
    /// Historical rows predate the durable pre-side-effect claim. Their
    /// Action Center state must be inspected before any new toast is allowed.
    #[default]
    LegacyUnclassified,
    /// A new-schema escalation has not yet entered the delivery pipeline.
    NotRequested,
    /// The intent and exact Applied projection generation are durable.
    /// `ToastNotifier.Show` has not been claimed, so retry after restart is safe.
    KnownUnsent {
        tag: String,
        projection_generation: TransitionGeneration,
        prepared_at_unix_ms: u64,
    },
    /// The `ToastNotifier.Show` side-effect boundary has been claimed. A crash
    /// from here has an unknown delivery outcome until Action Center history is
    /// inspected.
    StartedUnknown {
        tag: String,
        projection_generation: TransitionGeneration,
        owner_epoch: String,
        started_at_unix_ms: u64,
    },
    /// A separate Tag+Group history read proved the toast physically present.
    VerifiedPresent {
        tag: String,
        projection_generation: TransitionGeneration,
        history_count: u32,
        verified_at_unix_ms: u64,
    },
    /// The send path physically verified one exact payload row, then a
    /// separate read observed it absent (operator dismissal/removal). Delivery
    /// proof and current physical absence are both retained.
    VerifiedDismissed {
        tag: String,
        projection_generation: TransitionGeneration,
        history_count_at_delivery: u32,
        dismissed_readback: ToastHistoryReadback,
        verified_at_unix_ms: u64,
        dismissed_at_unix_ms: u64,
    },
    /// A matching reserved Tag+Group row physically existed before this caller
    /// reached the `ToastNotifier.Show` authorizer. The row is quarantined: no
    /// Show was invoked, no delivery is attributed to this escalation, and no
    /// automatic replay is allowed while the collision remains unresolved.
    PreShowCollision {
        tag: String,
        projection_generation: TransitionGeneration,
        error_code: String,
        error_message: String,
        history_readback: ToastHistoryReadback,
        classified_at_unix_ms: u64,
    },
    /// Delivery could not be proved. `side_effect_possible` prevents an unsafe
    /// automatic replay when WinRT may have displayed and then lost/dismissed it.
    Failed {
        tag: String,
        projection_generation: Option<TransitionGeneration>,
        error_code: String,
        error_message: String,
        /// Monotonic durable delivery proof that existed before the current
        /// physical-state classification failed. Historical rows used
        /// `tier0_fired` for this proof; a later mismatch must quarantine the
        /// physical row without erasing that verified fact.
        #[serde(default)]
        delivery_proven_before_failure: bool,
        side_effect_possible: bool,
        history_readback: Option<ToastHistoryReadback>,
        failed_at_unix_ms: u64,
    },
    /// Terminal cleanup was attempted but physical absence was not proved.
    RemovalFailed {
        tag: String,
        removal: ToastRemovalOutcome,
        attempt_count: u32,
        last_checked_at_unix_ms: u64,
        next_retry_at_unix_ms: u64,
        failed_at_unix_ms: u64,
    },
    /// Action Center removal separately read back zero matching Tag+Group rows.
    Removed {
        tag: String,
        removal: ToastRemovalOutcome,
        removed_at_unix_ms: u64,
    },
    /// Policy intentionally prevented Tier 0 before any delivery intent.
    Suppressed { reason: String, at_unix_ms: u64 },
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
    /// Authoritative Tier-0 intent/outcome state. The legacy boolean above is a
    /// compatibility projection only and never authorizes a side effect.
    #[serde(default)]
    pub tier0_delivery: Tier0ToastDelivery,
    /// Canonical WinRT LoadXml/GetXml payload digest for the reserved Tier-0
    /// namespace. It is durably prepared before the StartedUnknown boundary.
    /// Legacy rows have no trustworthy payload digest and remain `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier0_payload_sha256: Option<String>,
    /// Exact canonical payload frozen before the delivery side-effect claim.
    /// Recovery sends these bytes or fails closed; it never regenerates a
    /// possibly changed template and silently substitutes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier0_prepared_payload: Option<PreparedToastPayload>,
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
    /// Immutable channel order captured when this escalation opened. Current
    /// policy is looked up by stable ID, never by mutable list position.
    #[serde(default)]
    pub webhook_channel_ids: Vec<String>,
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

fn recent_index_key_parts(created_at_unix_ms: u64, escalation_id: &str) -> Vec<u8> {
    format!(
        "{RECENT_INDEX_PREFIX}{:016x}/{}",
        u64::MAX - created_at_unix_ms,
        escalation_id
    )
    .into_bytes()
}

fn recent_index_key(item: &EscalationItem) -> Vec<u8> {
    recent_index_key_parts(item.created_at_unix_ms, &item.escalation_id)
}

fn recent_index_row(item: &EscalationItem) -> (Vec<u8>, Vec<u8>) {
    (
        recent_index_key(item),
        item.escalation_id.as_bytes().to_vec(),
    )
}

fn audit_key(escalation_id: &str, at_unix_ms: u64, event_id: &str) -> Vec<u8> {
    format!("{AUDIT_PREFIX}{escalation_id}/{at_unix_ms:020}-{event_id}").into_bytes()
}

fn outbox_key(escalation_id: &str, ladder_index: u32) -> Vec<u8> {
    format!("{OUTBOX_PREFIX}{escalation_id}/{ladder_index:08}").into_bytes()
}

fn new_receiver_generation() -> String {
    format!("{RECEIVER_GENERATION_PREFIX}{}", Uuid::now_v7().simple())
}

fn new_legacy_channel_id() -> String {
    format!("legacy-{}", Uuid::now_v7().simple())
}

fn valid_receiver_generation(value: &str) -> bool {
    value
        .strip_prefix(RECEIVER_GENERATION_PREFIX)
        .is_some_and(|hex| {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn daemon_epoch() -> &'static str {
    static DAEMON_EPOCH: OnceLock<String> = OnceLock::new();
    DAEMON_EPOCH
        .get_or_init(|| format!("{DAEMON_EPOCH_PREFIX}{}", Uuid::now_v7().simple()))
        .as_str()
}

fn valid_daemon_epoch(value: &str) -> bool {
    value.strip_prefix(DAEMON_EPOCH_PREFIX).is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

static ACTIVE_TIER0_CLAIMS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();

fn active_tier0_claims() -> &'static Mutex<BTreeSet<String>> {
    ACTIVE_TIER0_CLAIMS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

#[derive(Debug)]
struct Tier0LiveClaim {
    escalation_id: String,
}

impl Tier0LiveClaim {
    fn register(escalation_id: &str) -> Result<Self, ErrorData> {
        let mut active = active_tier0_claims().lock().map_err(|poisoned| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!("Tier-0 live-claim registry is poisoned: {poisoned}"),
            )
        })?;
        if !active.insert(escalation_id.to_owned()) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 escalation {escalation_id} was claimed by two live callers before ToastNotifier.Show"
                ),
            ));
        }
        Ok(Self {
            escalation_id: escalation_id.to_owned(),
        })
    }

    fn is_registered(escalation_id: &str) -> Result<bool, ErrorData> {
        active_tier0_claims()
            .lock()
            .map(|active| active.contains(escalation_id))
            .map_err(|poisoned| {
                mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!("Tier-0 live-claim registry is poisoned: {poisoned}"),
                )
            })
    }
}

impl Drop for Tier0LiveClaim {
    fn drop(&mut self) {
        match active_tier0_claims().lock() {
            Ok(mut active) => {
                if !active.remove(&self.escalation_id) {
                    tracing::error!(
                        code = "ESCALATION_TIER0_LIVE_CLAIM_MISSING",
                        escalation_id = %self.escalation_id,
                        "Tier-0 live-claim guard dropped without a matching registry entry"
                    );
                }
            }
            Err(poisoned) => tracing::error!(
                code = "ESCALATION_TIER0_LIVE_CLAIM_POISONED",
                escalation_id = %self.escalation_id,
                detail = %poisoned,
                "Tier-0 live-claim guard could not clear the poisoned registry"
            ),
        }
    }
}

fn webhook_delivery_id(escalation_id: &str, channel_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"synapse.escalation.webhook-delivery.v2\0");
    digest.update(escalation_id.as_bytes());
    digest.update(
        u64::try_from(channel_id.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(channel_id.as_bytes());
    format!("whd2-{}", hex_bytes(&digest.finalize()))
}

fn webhook_channel_fingerprint(channel: &WebhookChannel) -> String {
    let mut digest = Sha256::new();
    digest.update(b"synapse.escalation.webhook-channel.v2\0");
    let public_origin = webhook_url_origin(&channel.url);
    for value in [
        channel.channel_id.as_bytes(),
        channel.name.as_bytes(),
        public_origin.as_bytes(),
        channel.idempotency_contract.as_str().as_bytes(),
    ] {
        digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(value);
    }
    digest.update([u8::from(channel.secret.is_some())]);
    hex_bytes(&digest.finalize())
}

fn webhook_public_endpoint_fingerprint(channel: &WebhookChannel) -> String {
    let mut digest = Sha256::new();
    digest.update(b"synapse.escalation.webhook-public-origin.v1\0");
    digest.update(webhook_url_origin(&channel.url).as_bytes());
    hex_bytes(&digest.finalize())
}

fn webhook_receiver_fingerprint(channel: &WebhookChannel) -> String {
    let mut digest = Sha256::new();
    digest.update(b"synapse.escalation.webhook-receiver.v1\0");
    digest.update(channel.url.as_bytes());
    digest.update(channel.idempotency_contract.as_str().as_bytes());
    hex_bytes(&digest.finalize())
}

fn webhook_url_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unparseable>".to_owned())
}

fn webhook_url_origin(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return "<unparseable>".to_owned();
    };
    let Some(host) = parsed.host_str() else {
        return "<unparseable>".to_owned();
    };
    let mut origin = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    origin
}

fn validate_outbox_record(key: &[u8], record: &WebhookOutboxRecord) -> Result<(), ErrorData> {
    validate_escalation_id(&record.escalation_id)?;
    if record.schema_version != SCHEMA_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook outbox has schema_version={} instead of {}: delivery_id={}",
                record.schema_version, SCHEMA_VERSION, record.delivery_id
            ),
        ));
    }
    let expected_key = outbox_key(&record.escalation_id, record.ladder_index);
    let expected_delivery_id = webhook_delivery_id(&record.escalation_id, &record.channel_id);
    if key != expected_key || record.delivery_id != expected_delivery_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook outbox identity mismatch: stored_key={} expected_key={} stored_delivery_id={:?} expected_delivery_id={:?}",
                hex_bytes(key),
                hex_bytes(&expected_key),
                record.delivery_id,
                expected_delivery_id
            ),
        ));
    }
    let body_digest = hex_bytes(&Sha256::digest(record.body_json.as_bytes()));
    let body = serde_json::from_str::<WebhookPayload>(&record.body_json).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook outbox body is not a valid closed payload: delivery_id={} error={error}",
                record.delivery_id
            ),
        )
    })?;
    let body_identity_matches = body.schema == "synapse.escalation.v1"
        && body.channel_id == record.channel_id
        && body.channel == record.channel_name
        && body.escalation_id == record.escalation_id
        && body.ladder_index == record.ladder_index;
    if record.body_sha256 != body_digest
        || record.attempt_number == 0
        || record.attempt_number > WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL
        || record.channel_name.is_empty()
        || record.channel_id.is_empty()
        || record.url_host.is_empty()
        || record.attempt_started_at_unix_ms == 0
        || record.updated_at_unix_ms < record.attempt_started_at_unix_ms
        || !body_identity_matches
        || record.channel_fingerprint_sha256.len() != 64
        || !record
            .channel_fingerprint_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || !valid_receiver_generation(&record.receiver_generation)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook outbox body/attempt invariant failed: delivery_id={} stored_body_sha256={} actual_body_sha256={} body_identity_matches={} attempt_number={} max_attempts={} attempt_started_at={} updated_at={}",
                record.delivery_id,
                record.body_sha256,
                body_digest,
                body_identity_matches,
                record.attempt_number,
                WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL,
                record.attempt_started_at_unix_ms,
                record.updated_at_unix_ms
            ),
        ));
    }
    if record
        .response_delivery_id
        .as_deref()
        .is_some_and(|response| response != record.delivery_id)
        && matches!(
            record.state,
            WebhookOutboxState::Accepted | WebhookOutboxState::TransientFailure
        )
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook outbox protocol-valid state has mismatched response delivery ID: delivery_id={} response_delivery_id={:?} state={:?}",
                record.delivery_id, record.response_delivery_id, record.state
            ),
        ));
    }
    let response_identity_proven =
        record.response_delivery_id.as_deref() == Some(record.delivery_id.as_str());
    let response_body_proven =
        record.response_body_sha256.as_deref() == Some(record.body_sha256.as_str());
    let response_committed = record.response_receipt_state == Some(WebhookReceiptState::Committed);
    let response_not_committed =
        record.response_receipt_state == Some(WebhookReceiptState::NotCommitted);
    let no_response_proof = record.response_delivery_id.is_none()
        && record.response_body_sha256.is_none()
        && record.response_receipt_state.is_none();
    let retryable_status = record.http_status.is_some_and(webhook_status_u16_retryable);
    let valid_state = match record.state {
        WebhookOutboxState::InFlight => {
            !record.contract_verified
                && record.http_status.is_none()
                && record.error.is_none()
                && no_response_proof
                && record.post_started_owner_epoch.is_none()
        }
        WebhookOutboxState::PostStarted => {
            record.contract_verified
                && record.http_status.is_none()
                && record.error.is_none()
                && no_response_proof
                && record
                    .post_started_owner_epoch
                    .as_deref()
                    .is_some_and(valid_daemon_epoch)
        }
        WebhookOutboxState::Accepted => {
            record.contract_verified
                && record
                    .http_status
                    .is_some_and(|status| (200..300).contains(&status))
                && record.error.is_none()
                && response_identity_proven
                && response_body_proven
                && response_committed
                && record.post_started_owner_epoch.is_none()
        }
        WebhookOutboxState::TransientFailure | WebhookOutboxState::RetryExhausted => {
            record.error.is_some()
                && record.post_started_owner_epoch.is_none()
                && if record.contract_verified {
                    retryable_status
                        && response_identity_proven
                        && response_body_proven
                        && response_not_committed
                } else {
                    no_response_proof && record.http_status.is_none_or(webhook_status_u16_retryable)
                }
        }
        WebhookOutboxState::Unknown => {
            record.contract_verified
                && record.http_status.is_none()
                && record.error.is_some()
                && no_response_proof
                && record.post_started_owner_epoch.is_none()
        }
        WebhookOutboxState::UnknownTerminal => {
            record.contract_verified
                && record.error.is_some()
                && no_response_proof
                && record.post_started_owner_epoch.is_none()
        }
        WebhookOutboxState::Abandoned => {
            !record.contract_verified
                && record.error.is_some()
                && record.http_status.is_none()
                && no_response_proof
                && record.post_started_owner_epoch.is_none()
        }
        WebhookOutboxState::TerminalFailure => {
            record.error.is_some()
                && record.post_started_owner_epoch.is_none()
                && if record.contract_verified {
                    response_identity_proven
                        && response_body_proven
                        && response_not_committed
                        && record
                            .http_status
                            .is_some_and(|status| !(200..300).contains(&status))
                } else {
                    !record.contract_verified && no_response_proof
                }
        }
    };
    if !valid_state
        || (record.contract_verified
            && record.idempotency_contract != WebhookIdempotencyContract::SynapseReceiptV1)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook outbox state/proof invariant failed: delivery_id={} state={:?} idempotency_contract={} contract_verified={} http_status={:?} error_present={} response_delivery_id={:?} response_body_sha256={:?} response_receipt_state={:?}",
                record.delivery_id,
                record.state,
                record.idempotency_contract.as_str(),
                record.contract_verified,
                record.http_status,
                record.error.is_some(),
                record.response_delivery_id,
                record.response_body_sha256,
                record.response_receipt_state
            ),
        ));
    }
    Ok(())
}

fn encode_outbox(record: &WebhookOutboxRecord) -> Result<Vec<u8>, ErrorData> {
    let key = outbox_key(&record.escalation_id, record.ladder_index);
    validate_outbox_record(&key, record)?;
    encode_json(record).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "webhook outbox encode failed for {}: {error}",
                record.delivery_id
            ),
        )
    })
}

fn read_outbox_revisioned(
    db: &Db,
    escalation_id: &str,
    ladder_index: u32,
) -> Result<Option<RevisionedWebhookOutbox>, ErrorData> {
    validate_escalation_id(escalation_id)?;
    let key = outbox_key(escalation_id, ladder_index);
    db.get_cf_revisioned(cf::CF_KV, &key)
        .map_err(storage_error)?
        .map(|revisioned| {
            let value = live_revisioned_value(
                &revisioned,
                &format!("webhook outbox {}", hex_bytes(&key)),
            )?;
            let record = decode_json::<WebhookOutboxRecord>(value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "webhook outbox decode failed for escalation_id={escalation_id} ladder_index={ladder_index}: {error}"
                    ),
                )
            })?;
            validate_outbox_record(&key, &record)?;
            Ok(RevisionedWebhookOutbox {
                record,
                revision_sha256: revisioned.revision_sha256,
            })
        })
        .transpose()
}

fn outbox_row(
    record: &WebhookOutboxRecord,
    expected_revision_sha256: Option<[u8; 32]>,
) -> Result<GuardedExtraRows, ErrorData> {
    let key = outbox_key(&record.escalation_id, record.ladder_index);
    let value = encode_outbox(record)?;
    Ok(GuardedExtraRows {
        rows: vec![(key.clone(), value)],
        guards: vec![RevisionGuard::new(key, expected_revision_sha256)],
    })
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

fn projection_anchor_digest(anchor: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"synapse.escalation.transition-projection.v2\0");
    digest.update(
        u64::try_from(anchor.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(anchor.as_bytes());
    hex_bytes(&digest.finalize())
}

fn projection_watermark_key(anchor: &str) -> Vec<u8> {
    format!(
        "{PROJECTION_WATERMARK_PREFIX}{}",
        projection_anchor_digest(anchor)
    )
    .into_bytes()
}

fn pending_projection_index_key(anchor: &str) -> Vec<u8> {
    format!(
        "{PENDING_PROJECTION_INDEX_PREFIX}{}",
        projection_anchor_digest(anchor)
    )
    .into_bytes()
}

fn is_lower_hex_exact(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_pending_projection_index(
    key: &[u8],
    record: &PendingTransitionProjectionIndex,
) -> Result<(), ErrorData> {
    let key_suffix = key.strip_prefix(PENDING_PROJECTION_INDEX_PREFIX.as_bytes());
    let key_shape_valid = key.len() == PENDING_PROJECTION_INDEX_KEY_LEN
        && key_suffix.is_some_and(|suffix| {
            suffix.len() == PROJECTION_ANCHOR_DIGEST_HEX_LEN
                && suffix
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        });
    let predecessor_revisions_valid = [
        record.cursor_predecessor_revision_sha256.as_deref(),
        record.index_predecessor_revision_sha256.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(|revision| is_lower_hex_exact(revision, 64));
    if record.schema_version != SCHEMA_VERSION
        || record.index_version != PENDING_PROJECTION_INDEX_VERSION
        || record.anchor.is_empty()
        || record.generation.journal_ts_ns == 0
        || !is_lower_hex_exact(&record.journal_key_hex, 24)
        || !is_lower_hex_exact(&record.journal_value_sha256, 64)
        || !is_lower_hex_exact(
            &record.cursor_key_hex,
            PROJECTION_WATERMARK_PREFIX.len() * 2 + 128,
        )
        || !is_lower_hex_exact(&record.cursor_value_sha256, 64)
        || !predecessor_revisions_valid
        || record.updated_at_unix_ms == 0
        || !key_shape_valid
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection index invariant failed: key={} key_len={} expected_key_len={PENDING_PROJECTION_INDEX_KEY_LEN} schema_version={} index_version={} anchor_empty={} generation={:?} journal_key_hex_len={} journal_value_sha256_len={} cursor_key_hex_len={} cursor_value_sha256_len={} predecessor_revisions_valid={} updated_at_unix_ms={}",
                hex_bytes(key),
                key.len(),
                record.schema_version,
                record.index_version,
                record.anchor.is_empty(),
                record.generation,
                record.journal_key_hex.len(),
                record.journal_value_sha256.len(),
                record.cursor_key_hex.len(),
                record.cursor_value_sha256.len(),
                predecessor_revisions_valid,
                record.updated_at_unix_ms
            ),
        ));
    }
    let expected_key = pending_projection_index_key(&record.anchor);
    let expected_cursor_key = projection_watermark_key(&record.anchor);
    let expected_journal_key = synapse_storage::agent_events::agent_event_key(
        record.generation.journal_ts_ns,
        record.generation.journal_seq,
    );
    if key != expected_key
        || record.cursor_key_hex != hex_bytes(&expected_cursor_key)
        || record.journal_key_hex != hex_bytes(&expected_journal_key)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection index identity mismatch: anchor={:?} stored_key={} expected_key={} stored_cursor_key={} expected_cursor_key={} stored_journal_key={} expected_journal_key={}",
                record.anchor,
                hex_bytes(key),
                hex_bytes(&expected_key),
                record.cursor_key_hex,
                hex_bytes(&expected_cursor_key),
                record.journal_key_hex,
                hex_bytes(&expected_journal_key)
            ),
        ));
    }
    Ok(())
}

fn validate_pending_projection_index_binding(
    index_key: &[u8],
    index: &PendingTransitionProjectionIndex,
    cursor_key: &[u8],
    cursor_value: &[u8],
    cursor: &TransitionProjectionWatermark,
) -> Result<(), ErrorData> {
    validate_pending_projection_index(index_key, index)?;
    validate_projection_watermark(cursor_key, cursor)?;
    let cursor_value_sha256 = hex_bytes(&Sha256::digest(cursor_value));
    if cursor.phase != TransitionProjectionPhase::Pending
        || index.anchor != cursor.anchor
        || index.generation != cursor.observed.generation
        || index.journal_key_hex != cursor.observed.journal_key_hex
        || index.journal_value_sha256 != cursor.observed.journal_value_sha256
        || index.cursor_key_hex != hex_bytes(cursor_key)
        || index.cursor_value_sha256 != cursor_value_sha256
        || index.updated_at_unix_ms != cursor.updated_at_unix_ms
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection index/cursor binding failed: anchor={:?} cursor_phase={:?} index_generation={:?} cursor_generation={:?} anchor_matches={} journal_key_matches={} journal_value_sha256_matches={} cursor_key_matches={} cursor_value_sha256_matches={} updated_at_matches={}",
                index.anchor,
                cursor.phase,
                index.generation,
                cursor.observed.generation,
                index.anchor == cursor.anchor,
                index.journal_key_hex == cursor.observed.journal_key_hex,
                index.journal_value_sha256 == cursor.observed.journal_value_sha256,
                index.cursor_key_hex == hex_bytes(cursor_key),
                index.cursor_value_sha256 == cursor_value_sha256,
                index.updated_at_unix_ms == cursor.updated_at_unix_ms
            ),
        ));
    }
    Ok(())
}

fn projection_audit_key(anchor: &str, generation: TransitionGeneration, event_id: &str) -> Vec<u8> {
    let digest = projection_anchor_digest(anchor);
    format!(
        "{PROJECTION_AUDIT_PREFIX}{digest}/{:020}-{:010}-{event_id}",
        generation.journal_ts_ns, generation.journal_seq
    )
    .into_bytes()
}

fn validate_projection_watermark(
    key: &[u8],
    record: &TransitionProjectionWatermark,
) -> Result<(), ErrorData> {
    if record.schema_version != SCHEMA_VERSION
        || record.cursor_version != PROJECTION_CURSOR_VERSION
        || record.anchor.is_empty()
        || record.observed.anchor != record.anchor
        || record.observed.generation.journal_ts_ns == 0
        || record.observed.journal_key_hex.len() != 24
        || !record
            .observed
            .journal_key_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || record.observed.journal_value_sha256.len() != 64
        || !record
            .observed
            .journal_value_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || record.observed.journal_value_hex.is_empty()
        || record.observed.journal_value_hex.len()
            > super::agent_events::MAX_AGENT_EVENT_VALUE_BYTES.saturating_mul(2)
        || !record.observed.journal_value_hex.len().is_multiple_of(2)
        || !record
            .observed
            .journal_value_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || record.observed.state_from.is_empty()
        || record.observed.state_to.is_empty()
        || record.observed.reason_code.is_empty()
        || AgentLifecycleState::parse(&record.observed.state_from).is_none()
        || AgentLifecycleState::parse(&record.observed.state_to).is_none()
        || record.updated_at_unix_ms == 0
        || record
            .last_applied_generation
            .is_some_and(|generation| generation.journal_ts_ns == 0)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection cursor invariant failed: key={} schema_version={} cursor_version={} anchor_empty={} journal_ts_ns={} journal_key_hex_len={} journal_value_sha256_len={} journal_value_hex_len={}",
                hex_bytes(key),
                record.schema_version,
                record.cursor_version,
                record.anchor.is_empty(),
                record.observed.generation.journal_ts_ns,
                record.observed.journal_key_hex.len(),
                record.observed.journal_value_sha256.len(),
                record.observed.journal_value_hex.len()
            ),
        ));
    }
    let expected_journal_key = synapse_storage::agent_events::agent_event_key(
        record.observed.generation.journal_ts_ns,
        record.observed.generation.journal_seq,
    );
    if record.observed.journal_key_hex != hex_bytes(&expected_journal_key) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection cursor generation/journal-key mismatch: anchor={:?} generation={:?} stored_key_hex={} expected_key_hex={}",
                record.anchor,
                record.observed.generation,
                record.observed.journal_key_hex,
                hex_bytes(&expected_journal_key)
            ),
        ));
    }
    validate_projection_journal_witness(record)?;
    let phase_valid = match record.phase {
        TransitionProjectionPhase::Pending => {
            record
                .last_applied_generation
                .is_none_or(|generation| generation < record.observed.generation)
                && record.applied_evidence.is_none()
        }
        TransitionProjectionPhase::Applied => {
            record.last_applied_generation == Some(record.observed.generation)
                && record.applied_evidence.is_some()
        }
    };
    if !phase_valid {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection cursor phase invariant failed: anchor={:?} phase={:?} observed={:?} last_applied={:?} applied_evidence_present={}",
                record.anchor,
                record.phase,
                record.observed.generation,
                record.last_applied_generation,
                record.applied_evidence.is_some()
            ),
        ));
    }
    let expected_key = projection_watermark_key(&record.anchor);
    if key != expected_key {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection watermark key/payload identity mismatch: stored_key={} expected_key={} anchor={:?}",
                hex_bytes(key),
                hex_bytes(&expected_key),
                record.anchor
            ),
        ));
    }
    if let Some(evidence) = &record.applied_evidence {
        validate_projection_applied_evidence_shape(record, evidence)?;
    }
    Ok(())
}

fn decode_projection_hex(field: &'static str, value: &str) -> Result<Vec<u8>, ErrorData> {
    if !value.len().is_multiple_of(2) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection {field} has odd hex length {}",
                value.len()
            ),
        ));
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = projection_hex_nibble(pair[0]).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("transition projection {field} is not lowercase hexadecimal"),
            )
        })?;
        let low = projection_hex_nibble(pair[1]).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("transition projection {field} is not lowercase hexadecimal"),
            )
        })?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

const fn projection_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn validate_projection_journal_witness(
    record: &TransitionProjectionWatermark,
) -> Result<(), ErrorData> {
    let journal_value =
        decode_projection_hex("journal_value_hex", &record.observed.journal_value_hex)?;
    let actual_digest = hex_bytes(&Sha256::digest(&journal_value));
    if actual_digest != record.observed.journal_value_sha256 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection journal witness digest mismatch: anchor={:?} generation={:?} expected={} actual={actual_digest}",
                record.anchor, record.observed.generation, record.observed.journal_value_sha256
            ),
        ));
    }
    let journal_record =
        decode_json::<synapse_core::AgentEventRecord>(&journal_value).map_err(storage_error)?;
    if journal_record.ts_ns != record.observed.generation.journal_ts_ns {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection journal witness timestamp/key mismatch: anchor={:?} generation={:?} record_ts_ns={}",
                record.anchor, record.observed.generation, journal_record.ts_ns
            ),
        ));
    }
    let transition = transition_from_projection_input(&record.observed)?;
    super::agent_state::validate_transition_record_identity(&journal_record, &transition)
        .map_err(storage_error)
}

fn projection_no_escalation_reason(transition: &StateTransition) -> Option<String> {
    operator_interrupt_suppressed_reason(transition)
        .map(|reason| format!("policy_suppressed:{reason}"))
        .or_else(|| {
            severity_for(transition.state_to)
                .is_none()
                .then(|| "state_not_escalatable".to_owned())
        })
}

fn validate_projection_applied_evidence_shape(
    record: &TransitionProjectionWatermark,
    evidence: &TransitionProjectionAppliedEvidence,
) -> Result<(), ErrorData> {
    let event_id_valid = evidence.event_id.len() == 32
        && evidence
            .event_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let digest_valid = evidence.audit_value_sha256.len() == 64
        && evidence
            .audit_value_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !event_id_valid
        || !digest_valid
        || evidence.at_unix_ms == 0
        || evidence.at_unix_ms != record.updated_at_unix_ms
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection applied evidence shape invalid: anchor={:?} event_id_len={} audit_digest_len={} at_unix_ms={} cursor_updated_at_unix_ms={}",
                record.anchor,
                evidence.event_id.len(),
                evidence.audit_value_sha256.len(),
                evidence.at_unix_ms,
                record.updated_at_unix_ms
            ),
        ));
    }
    let transition = transition_from_projection_input(&record.observed)?;
    let no_escalation_reason = projection_no_escalation_reason(&transition);
    match (&evidence.application, no_escalation_reason) {
        (
            TransitionProjectionApplication::Escalation {
                escalation_id,
                approval_id,
            },
            None,
        ) => {
            validate_escalation_id(escalation_id)?;
            if approval_id.trim().is_empty() || !approval_id.starts_with("apr1-") {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "transition projection applied evidence has invalid approval identity: anchor={:?} approval_id_len={}",
                        record.anchor,
                        approval_id.len()
                    ),
                ));
            }
        }
        (TransitionProjectionApplication::NoEscalation { reason }, Some(expected_reason))
            if *reason == expected_reason => {}
        (application, expected_reason) => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "transition projection applied evidence disposition conflicts with its transition: anchor={:?} application={application:?} expected_no_escalation_reason={expected_reason:?}",
                    record.anchor
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn projection_generation_for_anchor(
    db: &Db,
    anchor: &str,
) -> Result<Option<TransitionGeneration>, ErrorData> {
    read_projection_watermark(db, anchor)
        .map(|cursor| cursor.map(|cursor| cursor.record.observed.generation))
}

fn transition_projection_input(
    transition: &StateTransition,
    generation: TransitionGeneration,
    journal_key: &[u8],
    journal_value: &[u8],
) -> Result<TransitionProjectionInput, ErrorData> {
    let expected_key = synapse_storage::agent_events::agent_event_key(
        generation.journal_ts_ns,
        generation.journal_seq,
    );
    if journal_key != expected_key {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition journal generation/key mismatch before cursor publication: anchor={:?} generation={generation:?} journal_key={} expected_key={}",
                transition.anchor,
                hex_bytes(journal_key),
                hex_bytes(&expected_key)
            ),
        ));
    }
    Ok(TransitionProjectionInput {
        generation,
        journal_key_hex: hex_bytes(journal_key),
        journal_value_sha256: hex_bytes(&Sha256::digest(journal_value)),
        journal_value_hex: hex_bytes(journal_value),
        anchor: transition.anchor.clone(),
        spawn_id: transition.spawn_id.clone(),
        session_id: transition.session_id.clone(),
        state_from: transition.state_from.as_str().to_owned(),
        state_to: transition.state_to.as_str().to_owned(),
        reason_code: transition.reason_code.clone(),
        waiting_for: transition.waiting_for.clone(),
        runaway: transition.runaway,
        evidence: transition.evidence.clone(),
    })
}

fn transition_matches_projection_input(
    transition: &StateTransition,
    generation: TransitionGeneration,
    input: &TransitionProjectionInput,
) -> bool {
    input.generation == generation
        && input.anchor == transition.anchor
        && input.spawn_id == transition.spawn_id
        && input.session_id == transition.session_id
        && input.state_from == transition.state_from.as_str()
        && input.state_to == transition.state_to.as_str()
        && input.reason_code == transition.reason_code
        && input.waiting_for == transition.waiting_for
        && input.runaway == transition.runaway
        && input.evidence == transition.evidence
}

fn pending_projection_index_record(
    cursor_key: &[u8],
    cursor_value: &[u8],
    cursor: &TransitionProjectionWatermark,
    cursor_predecessor_revision_sha256: Option<[u8; 32]>,
    index_predecessor_revision_sha256: Option<[u8; 32]>,
) -> Result<(Vec<u8>, PendingTransitionProjectionIndex), ErrorData> {
    let index_key = pending_projection_index_key(&cursor.anchor);
    let record = PendingTransitionProjectionIndex {
        schema_version: SCHEMA_VERSION,
        index_version: PENDING_PROJECTION_INDEX_VERSION,
        anchor: cursor.anchor.clone(),
        generation: cursor.observed.generation,
        journal_key_hex: cursor.observed.journal_key_hex.clone(),
        journal_value_sha256: cursor.observed.journal_value_sha256.clone(),
        cursor_key_hex: hex_bytes(cursor_key),
        cursor_value_sha256: hex_bytes(&Sha256::digest(cursor_value)),
        cursor_predecessor_revision_sha256: cursor_predecessor_revision_sha256
            .as_ref()
            .map(|revision| hex_bytes(revision)),
        index_predecessor_revision_sha256: index_predecessor_revision_sha256
            .as_ref()
            .map(|revision| hex_bytes(revision)),
        updated_at_unix_ms: cursor.updated_at_unix_ms,
    };
    validate_pending_projection_index(&index_key, &record)?;
    validate_pending_projection_index_binding(
        &index_key,
        &record,
        cursor_key,
        cursor_value,
        cursor,
    )?;
    Ok((index_key, record))
}

pub(crate) struct PendingProjectionCursorWrite {
    pub(crate) cursor_key: Vec<u8>,
    pub(crate) cursor_value: Vec<u8>,
    pub(crate) expected_cursor_revision_sha256: Option<[u8; 32]>,
    pub(crate) index_key: Vec<u8>,
    pub(crate) index_value: Vec<u8>,
    pub(crate) expected_index_revision_sha256: Option<[u8; 32]>,
}

pub(crate) fn prepare_pending_projection_cursor(
    db: &Db,
    transition: &StateTransition,
    generation: TransitionGeneration,
    journal_key: &[u8],
    journal_value: &[u8],
    now_unix_ms: u64,
) -> Result<PendingProjectionCursorWrite, ErrorData> {
    let observed = transition_projection_input(transition, generation, journal_key, journal_value)?;
    let current = read_projection_watermark(db, &transition.anchor)?;
    let current_index = read_pending_projection_index(db, &transition.anchor)?;
    match (&current, &current_index) {
        (None, None) => {}
        (Some(cursor), Some(index))
            if cursor.record.phase == TransitionProjectionPhase::Pending =>
        {
            validate_pending_projection_index_binding(
                &pending_projection_index_key(&transition.anchor),
                &index.record,
                &projection_watermark_key(&transition.anchor),
                &cursor.value,
                &cursor.record,
            )?;
        }
        (Some(cursor), None) if cursor.record.phase == TransitionProjectionPhase::Applied => {}
        (Some(cursor), None) => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending transition projection cursor has no durable pending index before publication: anchor={:?} generation={:?} cursor_revision={}",
                    transition.anchor,
                    cursor.record.observed.generation,
                    hex_bytes(&cursor.revision_sha256)
                ),
            ));
        }
        (Some(cursor), Some(index)) => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Applied transition projection cursor retained a pending index before publication: anchor={:?} generation={:?} cursor_revision={} index_revision={}",
                    transition.anchor,
                    cursor.record.observed.generation,
                    hex_bytes(&cursor.revision_sha256),
                    hex_bytes(&index.revision_sha256)
                ),
            ));
        }
        (None, Some(index)) => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending transition projection index has no cursor before publication: anchor={:?} index_generation={:?} index_revision={}",
                    transition.anchor,
                    index.record.generation,
                    hex_bytes(&index.revision_sha256)
                ),
            ));
        }
    }
    if let Some(current) = &current {
        if current.record.observed.generation >= generation {
            let same = current.record.observed == observed;
            return Err(mcp_error(
                if same {
                    error_codes::STORAGE_WRITE_FAILED
                } else {
                    error_codes::STORAGE_CORRUPTED
                },
                format!(
                    "transition cursor publication is not a strict generation advance: anchor={:?} incoming={generation:?} current={:?} same_payload={same}",
                    transition.anchor, current.record.observed.generation
                ),
            ));
        }
    }
    let last_applied_generation = current
        .as_ref()
        .and_then(|current| match current.record.phase {
            TransitionProjectionPhase::Applied => Some(current.record.observed.generation),
            TransitionProjectionPhase::Pending => current.record.last_applied_generation,
        });
    let record = TransitionProjectionWatermark {
        schema_version: SCHEMA_VERSION,
        cursor_version: PROJECTION_CURSOR_VERSION,
        anchor: transition.anchor.clone(),
        observed,
        last_applied_generation,
        phase: TransitionProjectionPhase::Pending,
        applied_evidence: None,
        updated_at_unix_ms: now_unix_ms,
    };
    let key = projection_watermark_key(&transition.anchor);
    validate_projection_watermark(&key, &record)?;
    let value = encode_json(&record).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "transition projection cursor encode failed for anchor={:?}: {error}",
                transition.anchor
            ),
        )
    })?;
    let expected_cursor_revision_sha256 = current.as_ref().map(|current| current.revision_sha256);
    let expected_index_revision_sha256 = current_index
        .as_ref()
        .map(|current| current.revision_sha256);
    let (index_key, index_record) = pending_projection_index_record(
        &key,
        &value,
        &record,
        expected_cursor_revision_sha256,
        expected_index_revision_sha256,
    )?;
    let index_value = encode_json(&index_record).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "Pending transition projection index encode failed for anchor={:?}: {error}",
                transition.anchor
            ),
        )
    })?;
    Ok(PendingProjectionCursorWrite {
        cursor_key: key,
        cursor_value: value,
        expected_cursor_revision_sha256,
        index_key,
        index_value,
        expected_index_revision_sha256,
    })
}

fn read_projection_watermark(
    db: &Db,
    anchor: &str,
) -> Result<Option<RevisionedTransitionProjectionWatermark>, ErrorData> {
    let key = projection_watermark_key(anchor);
    db.get_cf_revisioned(cf::CF_KV, &key)
        .map_err(storage_error)?
        .map(|revisioned| {
            let value = live_revisioned_value(
                &revisioned,
                &format!("transition projection watermark {}", hex_bytes(&key)),
            )?;
            let record = decode_json::<TransitionProjectionWatermark>(value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "transition projection watermark decode failed for anchor={anchor:?}: {error}"
                    ),
                )
            })?;
            validate_projection_watermark(&key, &record)?;
            if record.anchor != anchor {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "transition projection watermark digest collision: requested_anchor={anchor:?} stored_anchor={:?}",
                        record.anchor
                    ),
                ));
            }
            Ok(RevisionedTransitionProjectionWatermark {
                record,
                value: value.to_vec(),
                revision_sha256: revisioned.revision_sha256,
            })
        })
        .transpose()
}

fn read_pending_projection_index(
    db: &Db,
    anchor: &str,
) -> Result<Option<RevisionedPendingTransitionProjectionIndex>, ErrorData> {
    let key = pending_projection_index_key(anchor);
    db.get_cf_revisioned(cf::CF_KV, &key)
        .map_err(storage_error)?
        .map(|revisioned| {
            let value = live_revisioned_value(
                &revisioned,
                &format!("Pending transition projection index {}", hex_bytes(&key)),
            )?;
            let record = decode_json::<PendingTransitionProjectionIndex>(value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "Pending transition projection index decode failed for anchor={anchor:?} key={}: {error}",
                        hex_bytes(&key)
                    ),
                )
            })?;
            validate_pending_projection_index(&key, &record)?;
            if record.anchor != anchor {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Pending transition projection index digest collision: requested_anchor={anchor:?} stored_anchor={:?} key={}",
                        record.anchor,
                        hex_bytes(&key)
                    ),
                ));
            }
            Ok(RevisionedPendingTransitionProjectionIndex {
                record,
                value: value.to_vec(),
                revision_sha256: revisioned.revision_sha256,
            })
        })
        .transpose()
}

fn read_and_validate_projection_index_for_cursor(
    db: &Db,
    cursor: &RevisionedTransitionProjectionWatermark,
) -> Result<Option<RevisionedPendingTransitionProjectionIndex>, ErrorData> {
    let index = read_pending_projection_index(db, &cursor.record.anchor)?;
    match (cursor.record.phase, index) {
        (TransitionProjectionPhase::Pending, Some(index)) => {
            validate_pending_projection_index_binding(
                &pending_projection_index_key(&cursor.record.anchor),
                &index.record,
                &projection_watermark_key(&cursor.record.anchor),
                &cursor.value,
                &cursor.record,
            )?;
            Ok(Some(index))
        }
        (TransitionProjectionPhase::Pending, None) => Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending transition projection cursor has no durable pending index: anchor={:?} generation={:?} cursor_revision={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                hex_bytes(&cursor.revision_sha256)
            ),
        )),
        (TransitionProjectionPhase::Applied, Some(index)) => Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection cursor retained a pending index: anchor={:?} generation={:?} cursor_revision={} index_revision={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                hex_bytes(&cursor.revision_sha256),
                hex_bytes(&index.revision_sha256)
            ),
        )),
        (TransitionProjectionPhase::Applied, None) => Ok(None),
    }
}

enum ProjectionMutationRows {
    Ready(GuardedExtraRows),
    Stale(Box<TransitionProjectionWatermark>),
    Applied,
}

fn projection_rows_for_mutation(
    db: &Db,
    transition: &StateTransition,
    generation: TransitionGeneration,
    _now_unix_ms: u64,
) -> Result<ProjectionMutationRows, ErrorData> {
    let Some(current) = read_projection_watermark(db, &transition.anchor)? else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection cursor is absent for committed journal generation: anchor={:?} generation={generation:?}",
                transition.anchor
            ),
        ));
    };
    let current_index = read_and_validate_projection_index_for_cursor(db, &current)?;
    if current.record.observed.generation > generation {
        return Ok(ProjectionMutationRows::Stale(Box::new(current.record)));
    }
    if current.record.observed.generation < generation {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition callback is newer than its durable cursor: anchor={:?} callback={generation:?} cursor={:?}",
                transition.anchor, current.record.observed.generation
            ),
        ));
    }
    if !transition_matches_projection_input(transition, generation, &current.record.observed) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition generation maps to conflicting complete projection payloads: anchor={:?} generation={generation:?}",
                transition.anchor
            ),
        ));
    }
    if current.record.phase == TransitionProjectionPhase::Applied {
        return Ok(ProjectionMutationRows::Applied);
    }
    let current_index = current_index.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection cursor lost its validated index before mutation planning: anchor={:?} generation={generation:?}",
                transition.anchor
            ),
        )
    })?;
    let record = current.record;
    let key = projection_watermark_key(&transition.anchor);
    validate_projection_watermark(&key, &record)?;
    let value = encode_json(&record).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "transition projection watermark encode failed for anchor={:?}: {error}",
                transition.anchor
            ),
        )
    })?;
    Ok(ProjectionMutationRows::Ready(GuardedExtraRows {
        rows: vec![
            (key.clone(), value),
            (
                pending_projection_index_key(&transition.anchor),
                current_index.value,
            ),
        ],
        guards: vec![
            RevisionGuard::new(key, Some(current.revision_sha256)),
            RevisionGuard::new(
                pending_projection_index_key(&transition.anchor),
                Some(current_index.revision_sha256),
            ),
        ],
    }))
}

fn persist_projection_watermark_only(
    db: &Db,
    transition: &StateTransition,
    generation: TransitionGeneration,
    now_unix_ms: u64,
    application: &TransitionProjectionApplication,
) -> Result<bool, ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        if let Some(current) = read_projection_watermark(db, &transition.anchor)? {
            let _current_index = read_and_validate_projection_index_for_cursor(db, &current)?;
            if current.record.observed.generation > generation {
                tracing::warn!(
                    code = "ESCALATION_TRANSITION_STALE",
                    anchor = %transition.anchor,
                    incoming_journal_ts_ns = generation.journal_ts_ns,
                    incoming_journal_seq = generation.journal_seq,
                    authoritative_journal_ts_ns = current.record.observed.generation.journal_ts_ns,
                    authoritative_journal_seq = current.record.observed.generation.journal_seq,
                    authoritative_state = %current.record.observed.state_to,
                    "older transition callback was rejected by the durable per-anchor projection cursor"
                );
                return Ok(false);
            }
            if current.record.observed.generation == generation
                && !transition_matches_projection_input(
                    transition,
                    generation,
                    &current.record.observed,
                )
            {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "transition apply request conflicts with the complete durable cursor payload: anchor={:?} generation={generation:?}",
                        transition.anchor
                    ),
                ));
            }
            if current.record.observed.generation == generation
                && current.record.phase == TransitionProjectionPhase::Applied
            {
                verify_applied_projection_evidence(db, &current.record)?;
                return Ok(true);
            }
        }
        let rows = match projection_rows_for_mutation(db, transition, generation, now_unix_ms)? {
            ProjectionMutationRows::Ready(rows) => rows,
            ProjectionMutationRows::Stale(authoritative) => {
                tracing::warn!(
                    code = "ESCALATION_TRANSITION_STALE",
                    anchor = %transition.anchor,
                    incoming_journal_ts_ns = generation.journal_ts_ns,
                    incoming_journal_seq = generation.journal_seq,
                    authoritative_journal_ts_ns = authoritative.observed.generation.journal_ts_ns,
                    authoritative_journal_seq = authoritative.observed.generation.journal_seq,
                    authoritative_state = %authoritative.observed.state_to,
                    "older transition callback was rejected before cursor completion"
                );
                return Ok(false);
            }
            ProjectionMutationRows::Applied => {
                verify_applied_projection_for_transition(db, transition, generation)?;
                return Ok(true);
            }
        };
        let GuardedExtraRows { mut rows, guards } = rows;
        let watermark_key = projection_watermark_key(&transition.anchor);
        let pending_index_key = pending_projection_index_key(&transition.anchor);
        let watermark_row_index = rows
            .iter()
            .position(|(key, _value)| *key == watermark_key)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "projection cursor mutation produced no watermark row",
                )
            })?;
        let pending_index_row_index = rows
            .iter()
            .position(|(key, _value)| *key == pending_index_key)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "projection cursor mutation produced no Pending index row",
                )
            })?;
        let pending_value = rows[watermark_row_index].1.clone();
        let pending_index_value = rows[pending_index_row_index].1.clone();
        let pending_index = decode_json::<PendingTransitionProjectionIndex>(&pending_index_value)
            .map_err(|error| {
            mcp_error(
                error.code(),
                format!("Pending transition projection index decode failed: {error}"),
            )
        })?;
        let cursor_guard_index = guards
            .iter()
            .position(|guard| guard.key == watermark_key)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "projection completion produced no cursor revision guard",
                )
            })?;
        let pending_index_guard_index = guards
            .iter()
            .position(|guard| guard.key == pending_index_key)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "projection completion produced no Pending index revision guard",
                )
            })?;
        if guards.len() != 2 || rows.len() != 2 {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "standalone projection completion produced an invalid mutation shape: guards={} rows={}",
                    guards.len(),
                    rows.len()
                ),
            ));
        }
        let mut applied =
            decode_json::<TransitionProjectionWatermark>(&pending_value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("pending transition projection cursor decode failed: {error}"),
                )
            })?;
        validate_pending_projection_index_binding(
            &pending_index_key,
            &pending_index,
            &watermark_key,
            &pending_value,
            &applied,
        )?;
        rows.remove(pending_index_row_index);
        let remaining_watermark_row_index = rows
            .iter()
            .position(|(key, _value)| *key == watermark_key)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "projection cursor row disappeared while planning index deletion",
                )
            })?;
        if rows.len() != 1 {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "projection completion retained unexpected rows after removing its Pending index: rows={}",
                    rows.len()
                ),
            ));
        }
        applied.phase = TransitionProjectionPhase::Applied;
        applied.last_applied_generation = Some(generation);
        applied.updated_at_unix_ms = now_unix_ms;
        let event_id = Uuid::now_v7().simple().to_string();
        let audit_key = projection_audit_key(&transition.anchor, generation, &event_id);
        let audit = TransitionProjectionAuditRecord {
            schema_version: SCHEMA_VERSION,
            cursor_version: PROJECTION_CURSOR_VERSION,
            event: "transition_projection_applied".to_owned(),
            event_id: event_id.clone(),
            anchor: transition.anchor.clone(),
            generation,
            state_to: transition.state_to.as_str().to_owned(),
            reason_code: transition.reason_code.clone(),
            journal_value_sha256: applied.observed.journal_value_sha256.clone(),
            at_unix_ms: now_unix_ms,
            application: application.clone(),
        };
        let audit_value = encode_json(&audit).map_err(|error| {
            mcp_error(
                error.code(),
                format!("transition projection audit encode failed: {error}"),
            )
        })?;
        applied.applied_evidence = Some(TransitionProjectionAppliedEvidence {
            event_id,
            at_unix_ms: now_unix_ms,
            audit_value_sha256: hex_bytes(&Sha256::digest(&audit_value)),
            application: application.clone(),
        });
        validate_projection_watermark(&watermark_key, &applied)?;
        let watermark_value = encode_json(&applied).map_err(|error| {
            mcp_error(
                error.code(),
                format!("applied transition projection cursor encode failed: {error}"),
            )
        })?;
        rows[remaining_watermark_row_index]
            .1
            .clone_from(&watermark_value);
        rows.push((audit_key.clone(), audit_value.clone()));
        let outcome = db
            .mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                guards,
                [pending_index_key.clone()],
                rows,
            )
            .map_err(storage_error)?;
        if !outcome.applied {
            tracing::info!(
                code = "ESCALATION_PROJECTION_WATERMARK_REVISION_RETRY",
                anchor = %transition.anchor,
                incoming_journal_ts_ns = generation.journal_ts_ns,
                incoming_journal_seq = generation.journal_seq,
                revision_attempt,
                observed_seq = outcome.committed_seq,
                "standalone projection watermark lost a revision race; rereading authoritative generation"
            );
            continue;
        }
        if outcome.committed_revisions_sha256.len() != 2
            || outcome.committed_revisions_sha256[pending_index_guard_index].is_some()
        {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "projection completion returned invalid cursor/index revision shape: committed_seq={} revisions={} cursor_guard_index={} index_guard_index={} index_revision_present={}",
                    outcome.committed_seq,
                    outcome.committed_revisions_sha256.len(),
                    cursor_guard_index,
                    pending_index_guard_index,
                    outcome
                        .committed_revisions_sha256
                        .get(pending_index_guard_index)
                        .is_some_and(|revision| revision.is_some())
                ),
            ));
        }
        let committed_revision = outcome.committed_revisions_sha256[cursor_guard_index]
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                "projection cursor commit returned no applied cursor revision: committed_seq={} revisions={}",
                        outcome.committed_seq,
                        outcome.committed_revisions_sha256.len()
                    ),
                )
            })?;
        let readback = db
            .get_cf_revisioned(cf::CF_KV, &watermark_key)
            .map_err(storage_error)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    "projection cursor absent immediately after applied commit",
                )
            })?;
        let readback_value = live_revisioned_value(&readback, "projection cursor readback")?;
        if readback.revision_sha256 != committed_revision
            || readback_value != watermark_value.as_slice()
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "projection cursor physical readback differed after applied commit: revision_matches={} bytes_match={}",
                    readback.revision_sha256 == committed_revision,
                    readback_value == watermark_value.as_slice()
                ),
            ));
        }
        let authoritative =
            decode_json::<TransitionProjectionWatermark>(readback_value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("projection cursor readback decode failed: {error}"),
                )
            })?;
        validate_projection_watermark(&watermark_key, &authoritative)?;
        if authoritative.observed.generation != generation
            || authoritative.phase != TransitionProjectionPhase::Applied
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "projection cursor readback was not the exact applied generation: expected={generation:?} actual={:?} phase={:?}",
                    authoritative.observed.generation, authoritative.phase
                ),
            ));
        }
        if let Some(index_readback) = db
            .get_cf_revisioned(cf::CF_KV, &pending_index_key)
            .map_err(storage_error)?
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending projection index remained physically readable after atomic Applied completion: anchor={:?} generation={generation:?} index_key={} index_revision={} live_value_present={}",
                    transition.anchor,
                    hex_bytes(&pending_index_key),
                    hex_bytes(&index_readback.revision_sha256),
                    index_readback.value.is_some()
                ),
            ));
        }
        let audit_readback = db
            .get_cf_revisioned(cf::CF_KV, &audit_key)
            .map_err(storage_error)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    "projection audit absent immediately after standalone commit",
                )
            })?;
        let audit_readback_value =
            live_revisioned_value(&audit_readback, "projection audit commit readback")?;
        if audit_readback_value != audit_value {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                "projection audit bytes differed immediately after standalone commit",
            ));
        }
        tracing::debug!(
            code = "ESCALATION_PROJECTION_AUDIT_READBACK",
            anchor = %transition.anchor,
            journal_ts_ns = generation.journal_ts_ns,
            journal_seq = generation.journal_seq,
            audit_revision = %hex_bytes(&audit_readback.revision_sha256),
            audit_value_len = audit_readback_value.len(),
            "readback=CF_KV exact physical transition projection audit"
        );
        verify_applied_projection_evidence(db, &authoritative)?;
        tracing::info!(
            code = "ESCALATION_PROJECTION_CURSOR_APPLIED",
            anchor = %transition.anchor,
            state_to = transition.state_to.as_str(),
            journal_ts_ns = generation.journal_ts_ns,
            journal_seq = generation.journal_seq,
            committed_seq = outcome.committed_seq,
            watermark_key = %String::from_utf8_lossy(&watermark_key),
            audit_key = %String::from_utf8_lossy(&audit_key),
            "readback=CF_KV transition projection cursor marked applied with an audit row"
        );
        return Ok(true);
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "transition projection cursor for anchor {:?} could not acquire a stable revision after {ACK_REVISION_MAX_ATTEMPTS} attempts",
            transition.anchor
        ),
    ))
}

fn expected_projection_audit(
    cursor: &TransitionProjectionWatermark,
    evidence: &TransitionProjectionAppliedEvidence,
) -> TransitionProjectionAuditRecord {
    TransitionProjectionAuditRecord {
        schema_version: SCHEMA_VERSION,
        cursor_version: PROJECTION_CURSOR_VERSION,
        event: "transition_projection_applied".to_owned(),
        event_id: evidence.event_id.clone(),
        anchor: cursor.anchor.clone(),
        generation: cursor.observed.generation,
        state_to: cursor.observed.state_to.clone(),
        reason_code: cursor.observed.reason_code.clone(),
        journal_value_sha256: cursor.observed.journal_value_sha256.clone(),
        at_unix_ms: evidence.at_unix_ms,
        application: evidence.application.clone(),
    }
}

fn verify_applied_projection_evidence(
    db: &Db,
    cursor: &TransitionProjectionWatermark,
) -> Result<(), ErrorData> {
    validate_projection_watermark(&projection_watermark_key(&cursor.anchor), cursor)?;
    if cursor.phase != TransitionProjectionPhase::Applied {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection evidence requested for non-Applied cursor: anchor={:?} phase={:?}",
                cursor.anchor, cursor.phase
            ),
        ));
    }
    if let Some(index) = read_pending_projection_index(db, &cursor.anchor)? {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection cursor retained durable Pending work: anchor={:?} generation={:?} index_key={} index_revision={}",
                cursor.anchor,
                cursor.observed.generation,
                hex_bytes(&pending_projection_index_key(&cursor.anchor)),
                hex_bytes(&index.revision_sha256)
            ),
        ));
    }
    let evidence = cursor.applied_evidence.as_ref().ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection cursor has no durable evidence: anchor={:?} generation={:?}",
                cursor.anchor, cursor.observed.generation
            ),
        )
    })?;
    let audit = expected_projection_audit(cursor, evidence);
    let expected_value = encode_json(&audit).map_err(|error| {
        mcp_error(
            error.code(),
            format!("expected transition projection audit encode failed: {error}"),
        )
    })?;
    let expected_digest = hex_bytes(&Sha256::digest(&expected_value));
    if expected_digest != evidence.audit_value_sha256 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection cursor audit digest is not self-consistent: anchor={:?} generation={:?} expected={} stored={}",
                cursor.anchor,
                cursor.observed.generation,
                expected_digest,
                evidence.audit_value_sha256
            ),
        ));
    }
    let audit_key = projection_audit_key(
        &cursor.anchor,
        cursor.observed.generation,
        &evidence.event_id,
    );
    let actual = db
        .get_cf_revisioned(cf::CF_KV, &audit_key)
        .map_err(storage_error)?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Applied transition projection cursor references a missing audit: anchor={:?} generation={:?} audit_key={}",
                    cursor.anchor,
                    cursor.observed.generation,
                    String::from_utf8_lossy(&audit_key)
                ),
            )
        })?;
    let actual_value = live_revisioned_value(&actual, "Applied projection audit evidence")?;
    if actual_value != expected_value {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection audit bytes differ from cursor evidence: anchor={:?} generation={:?} expected_len={} actual_len={}",
                cursor.anchor,
                cursor.observed.generation,
                expected_value.len(),
                actual_value.len()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn projection_journal_witness_for_anchor(
    db: &Db,
    anchor: &str,
) -> Result<Option<TransitionProjectionJournalWitness>, ErrorData> {
    let Some(cursor) = read_projection_watermark(db, anchor)? else {
        return Ok(None);
    };
    verify_applied_projection_evidence(db, &cursor.record)?;
    let journal_key =
        decode_projection_hex("journal_key_hex", &cursor.record.observed.journal_key_hex)?;
    let expected_journal_key = synapse_storage::agent_events::agent_event_key(
        cursor.record.observed.generation.journal_ts_ns,
        cursor.record.observed.generation.journal_seq,
    );
    if journal_key != expected_journal_key {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection witness key differs from its generation: anchor={anchor:?} generation={:?} stored_key={} expected_key={}",
                cursor.record.observed.generation,
                hex_bytes(&journal_key),
                hex_bytes(&expected_journal_key)
            ),
        ));
    }
    let journal_value = decode_projection_hex(
        "journal_value_hex",
        &cursor.record.observed.journal_value_hex,
    )?;
    Ok(Some(TransitionProjectionJournalWitness {
        generation: cursor.record.observed.generation,
        journal_key,
        journal_value,
        watermark_revision_sha256: cursor.revision_sha256,
    }))
}

fn verify_applied_projection_for_transition(
    db: &Db,
    transition: &StateTransition,
    generation: TransitionGeneration,
) -> Result<(), ErrorData> {
    let current = read_projection_watermark(db, &transition.anchor)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection cursor disappeared: anchor={:?} generation={generation:?}",
                transition.anchor
            ),
        )
    })?;
    if current.record.observed.generation != generation
        || !transition_matches_projection_input(transition, generation, &current.record.observed)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied transition projection evidence belongs to another generation: anchor={:?} expected={generation:?} actual={:?}",
                transition.anchor, current.record.observed.generation
            ),
        ));
    }
    verify_applied_projection_evidence(db, &current.record)
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

fn tier0_delivery_tag(delivery: &Tier0ToastDelivery) -> Option<&str> {
    match delivery {
        Tier0ToastDelivery::KnownUnsent { tag, .. }
        | Tier0ToastDelivery::StartedUnknown { tag, .. }
        | Tier0ToastDelivery::VerifiedPresent { tag, .. }
        | Tier0ToastDelivery::VerifiedDismissed { tag, .. }
        | Tier0ToastDelivery::PreShowCollision { tag, .. }
        | Tier0ToastDelivery::Failed { tag, .. }
        | Tier0ToastDelivery::RemovalFailed { tag, .. }
        | Tier0ToastDelivery::Removed { tag, .. } => Some(tag),
        Tier0ToastDelivery::LegacyUnclassified
        | Tier0ToastDelivery::NotRequested
        | Tier0ToastDelivery::Suppressed { .. } => None,
    }
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn tier0_removal_retry_delay_ms(attempt_count: u32) -> u64 {
    let exponent = attempt_count.saturating_sub(1).min(6);
    TIER0_REMOVAL_RETRY_BASE_MS
        .saturating_mul(1_u64 << exponent)
        .min(TIER0_REMOVAL_RETRY_MAX_MS)
}

fn tier0_group_for_exact_tag(item: &EscalationItem, tag: &str) -> Option<&'static str> {
    if tag == escalation_toast_tag(&item.escalation_id) {
        Some(SYNAPSE_ESCALATION_TOAST_GROUP)
    } else if tag == legacy_escalation_toast_tag(&item.escalation_id) {
        Some(SYNAPSE_TOAST_GROUP)
    } else {
        None
    }
}

fn tier0_payload_binding_valid(item: &EscalationItem, tag: &str) -> bool {
    tier0_group_for_exact_tag(item, tag).is_some() && tier0_prepared_binding_valid(item, false)
}

fn tier0_removal_payload_binding_valid(item: &EscalationItem, tag: &str) -> bool {
    tier0_group_for_exact_tag(item, tag).is_some() && tier0_prepared_binding_valid(item, true)
}

fn tier0_prepared_request_binding_valid(
    item: &EscalationItem,
    prepared: &PreparedToastPayload,
) -> bool {
    tier0_notify_params_for_prepared(item, prepared)
        .is_some_and(|params| prepared_toast_payload_matches_request(prepared, &params, &[]))
}

fn tier0_prepared_binding_valid(item: &EscalationItem, allow_absent: bool) -> bool {
    match (
        item.tier0_payload_sha256.as_deref(),
        item.tier0_prepared_payload.as_ref(),
    ) {
        (None, None) => allow_absent,
        (Some(payload_sha256), Some(prepared)) => {
            is_canonical_sha256(payload_sha256)
                && prepared.payload_sha256 == payload_sha256
                && prepared.suppress_popup == item.tier0_quiet_digest
                && ((prepared_toast_payload_valid(prepared)
                    && tier0_prepared_request_binding_valid(item, prepared))
                    || legacy_prepared_toast_payload_v1_valid(prepared))
        }
        _ => false,
    }
}

fn tier0_reconciliation_identity(
    item: &EscalationItem,
) -> Result<(String, &'static str), ErrorData> {
    let tag = tier0_delivery_tag(&item.tier0_delivery).map_or_else(
        || {
            if item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified {
                legacy_escalation_toast_tag(&item.escalation_id)
            } else {
                escalation_toast_tag(&item.escalation_id)
            }
        },
        str::to_owned,
    );
    let group = tier0_group_for_exact_tag(item, &tag).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 delivery has no reconcilable toast identity: escalation_id={} delivery={:?}",
                item.escalation_id, item.tier0_delivery
            ),
        )
    })?;
    Ok((tag, group))
}

fn tier0_readback_contract_valid(
    item: &EscalationItem,
    expected_tag: &str,
    readback: &ToastHistoryReadback,
) -> bool {
    if !tier0_readback_identity_shape_valid(item, expected_tag, readback) {
        return false;
    }
    match readback.history_count {
        0 => true,
        1 => {
            let payload_matches = item
                .tier0_payload_sha256
                .as_ref()
                .is_some_and(|expected| readback.payload_sha256s.first() == Some(expected));
            let expected_expiration = match tier0_group_for_exact_tag(item, expected_tag) {
                Some(SYNAPSE_ESCALATION_TOAST_GROUP) => Some(item.expires_at_unix_ms),
                Some(SYNAPSE_TOAST_GROUP) => None,
                _ => return false,
            };
            payload_matches && readback.expiration_unix_ms.first() == Some(&expected_expiration)
        }
        _ => false,
    }
}

fn tier0_readback_identity_shape_valid(
    item: &EscalationItem,
    expected_tag: &str,
    readback: &ToastHistoryReadback,
) -> bool {
    let Some(expected_group) = tier0_group_for_exact_tag(item, expected_tag) else {
        return false;
    };
    let Ok(history_count) = usize::try_from(readback.history_count) else {
        return false;
    };
    if readback.aumid != SYNAPSE_AUMID
        || readback.group != expected_group
        || readback.tag != expected_tag
        || readback.present != (history_count > 0)
        || readback.payload_sha256s.len() != history_count
        || readback.expiration_unix_ms.len() != history_count
        || readback
            .payload_sha256s
            .iter()
            .any(|digest| !is_canonical_sha256(digest))
    {
        return false;
    }
    true
}

fn tier0_removal_identity_valid(
    item: &EscalationItem,
    tag: &str,
    removal: &ToastRemovalOutcome,
) -> bool {
    tier0_group_for_exact_tag(item, tag).is_some_and(|group| {
        removal.aumid == SYNAPSE_AUMID && removal.tag == tag && removal.group == group
    })
}

fn validate_tier0_delivery(item: &EscalationItem) -> Result<(), ErrorData> {
    if tier0_delivery_tag(&item.tier0_delivery)
        .is_some_and(|tag| tier0_group_for_exact_tag(item, tag).is_none())
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 toast tag is outside both the reserved and exact legacy escalation identities: escalation_id={} delivery={:?}",
                item.escalation_id, item.tier0_delivery
            ),
        ));
    }
    let state_valid = match &item.tier0_delivery {
        Tier0ToastDelivery::LegacyUnclassified => {
            item.tier0_payload_sha256.is_none() && item.tier0_prepared_payload.is_none()
                || tier0_prepared_binding_valid(item, false)
        }
        Tier0ToastDelivery::NotRequested => {
            !item.tier0_fired
                && item.tier0_toast_removed.is_none()
                && item.tier0_suppressed_reason.is_none()
                && item.tier0_payload_sha256.is_none()
                && item.tier0_prepared_payload.is_none()
        }
        Tier0ToastDelivery::KnownUnsent {
            tag,
            projection_generation,
            prepared_at_unix_ms,
        } => {
            !item.tier0_fired
                && item.tier0_toast_removed.is_none()
                && *tag == escalation_toast_tag(&item.escalation_id)
                && tier0_payload_binding_valid(item, tag)
                && projection_generation.journal_ts_ns > 0
                && *prepared_at_unix_ms >= item.created_at_unix_ms
        }
        Tier0ToastDelivery::StartedUnknown {
            tag,
            projection_generation,
            owner_epoch,
            started_at_unix_ms,
        } => {
            !item.tier0_fired
                && item.tier0_toast_removed.is_none()
                && *tag == escalation_toast_tag(&item.escalation_id)
                && tier0_payload_binding_valid(item, tag)
                && projection_generation.journal_ts_ns > 0
                && valid_daemon_epoch(owner_epoch)
                && *started_at_unix_ms >= item.created_at_unix_ms
        }
        Tier0ToastDelivery::VerifiedPresent {
            tag,
            projection_generation,
            history_count,
            verified_at_unix_ms,
        } => {
            item.tier0_fired
                && item.tier0_toast_removed.is_none()
                && tier0_payload_binding_valid(item, tag)
                && projection_generation.journal_ts_ns > 0
                && *history_count == 1
                && *verified_at_unix_ms >= item.created_at_unix_ms
        }
        Tier0ToastDelivery::VerifiedDismissed {
            tag,
            projection_generation,
            history_count_at_delivery,
            dismissed_readback,
            verified_at_unix_ms,
            dismissed_at_unix_ms,
        } => {
            item.tier0_fired
                && item.tier0_toast_removed.is_none()
                && tier0_payload_binding_valid(item, tag)
                && projection_generation.journal_ts_ns > 0
                && *history_count_at_delivery == 1
                && *verified_at_unix_ms >= item.created_at_unix_ms
                && *dismissed_at_unix_ms >= *verified_at_unix_ms
                && tier0_readback_contract_valid(item, tag, dismissed_readback)
                && !dismissed_readback.present
                && dismissed_readback.history_count == 0
        }
        Tier0ToastDelivery::PreShowCollision {
            tag,
            projection_generation,
            error_code,
            error_message,
            history_readback,
            classified_at_unix_ms,
        } => {
            !item.tier0_fired
                && item.tier0_toast_removed.is_none()
                && *tag == escalation_toast_tag(&item.escalation_id)
                && tier0_payload_binding_valid(item, tag)
                && projection_generation.journal_ts_ns > 0
                && !error_code.is_empty()
                && !error_message.is_empty()
                && *classified_at_unix_ms >= item.created_at_unix_ms
                && tier0_readback_identity_shape_valid(item, tag, history_readback)
                && history_readback.present
                && history_readback.history_count > 0
                && !tier0_readback_contract_valid(item, tag, history_readback)
        }
        Tier0ToastDelivery::Failed {
            error_code,
            error_message,
            delivery_proven_before_failure,
            side_effect_possible,
            history_readback,
            failed_at_unix_ms,
            tag,
            projection_generation,
        } => {
            item.tier0_fired == *delivery_proven_before_failure
                && item.tier0_toast_removed.is_none()
                && tier0_payload_binding_valid(item, tag)
                && projection_generation
                    .as_ref()
                    .is_some_and(|generation| generation.journal_ts_ns > 0)
                && !error_code.is_empty()
                && !error_message.is_empty()
                && *side_effect_possible
                && *failed_at_unix_ms >= item.created_at_unix_ms
                && history_readback.as_ref().is_some_and(|readback| {
                    tier0_readback_identity_shape_valid(item, tag, readback)
                        && ((!readback.present && readback.history_count == 0)
                            || (readback.present
                                && (readback.history_count != 1
                                    || !tier0_readback_contract_valid(item, tag, readback))))
                })
        }
        Tier0ToastDelivery::RemovalFailed {
            tag,
            removal,
            attempt_count,
            last_checked_at_unix_ms,
            next_retry_at_unix_ms,
            failed_at_unix_ms,
        } => {
            item.status != EscalationStatus::Pending
                && tier0_removal_payload_binding_valid(item, tag)
                && tier0_removal_identity_valid(item, tag, removal)
                && !removal.removed
                && !removal.already_absent
                && removal.after_count != Some(0)
                && !removal.status.is_empty()
                && removal
                    .error_code
                    .as_deref()
                    .is_some_and(|code| !code.is_empty())
                && removal
                    .error_message
                    .as_deref()
                    .is_some_and(|message| !message.is_empty())
                && item.tier0_toast_removed.as_ref() == Some(removal)
                && *attempt_count > 0
                && *last_checked_at_unix_ms >= *failed_at_unix_ms
                && *next_retry_at_unix_ms
                    == last_checked_at_unix_ms
                        .saturating_add(tier0_removal_retry_delay_ms(*attempt_count))
        }
        Tier0ToastDelivery::Removed { tag, removal, .. } => {
            let disposition_valid = if removal.removed {
                !removal.already_absent
                    && removal.status == "removed"
                    && removal.before_count == Some(1)
            } else {
                removal.already_absent
                    && removal.status == "not_present"
                    && removal.before_count == Some(0)
            };
            item.status != EscalationStatus::Pending
                && tier0_removal_payload_binding_valid(item, tag)
                && tier0_removal_identity_valid(item, tag, removal)
                && disposition_valid
                && removal.after_count == Some(0)
                && removal.error_code.is_none()
                && removal.error_message.is_none()
                && item.tier0_toast_removed.as_ref() == Some(removal)
                && (!removal.removed || item.tier0_fired)
        }
        Tier0ToastDelivery::Suppressed { reason, .. } => {
            !item.tier0_fired
                && item.tier0_toast_removed.is_none()
                && item.tier0_payload_sha256.is_none()
                && item.tier0_prepared_payload.is_none()
                && !reason.is_empty()
                && item.tier0_suppressed_reason.as_deref() == Some(reason)
        }
    };
    if !state_valid {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 delivery state disagrees with item compatibility/physical-proof fields: escalation_id={} status={} tier0_fired={} removal_present={} suppressed_reason={:?} delivery={:?}",
                item.escalation_id,
                item.status.as_str(),
                item.tier0_fired,
                item.tier0_toast_removed.is_some(),
                item.tier0_suppressed_reason,
                item.tier0_delivery
            ),
        ));
    }
    Ok(())
}

fn validate_item(item: &EscalationItem) -> Result<(), ErrorData> {
    validate_escalation_id(&item.escalation_id)?;
    if item.schema_version != SCHEMA_VERSION
        || item.approval_id.is_empty()
        || item.anchor.is_empty()
        || item.attention_state.is_empty()
        || item.expires_at_unix_ms < item.created_at_unix_ms
        || item.updated_at_unix_ms < item.created_at_unix_ms
        || item.ladder_index > MAX_WEBHOOKS as u32
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation item invariant failed: escalation_id={} schema_version={} approval_empty={} anchor_empty={} state_empty={} created_at={} updated_at={} expires_at={} ladder_index={}",
                item.escalation_id,
                item.schema_version,
                item.approval_id.is_empty(),
                item.anchor.is_empty(),
                item.attention_state.is_empty(),
                item.created_at_unix_ms,
                item.updated_at_unix_ms,
                item.expires_at_unix_ms,
                item.ladder_index
            ),
        ));
    }
    if item.status != EscalationStatus::Pending && item.next_escalate_at_unix_ms.is_some() {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "non-pending escalation retains next_escalate_at: escalation_id={} status={}",
                item.escalation_id,
                item.status.as_str()
            ),
        ));
    }
    if item.webhook_channel_ids.len() > MAX_WEBHOOKS
        || item
            .webhook_channel_ids
            .iter()
            .any(|channel_id| channel_id.is_empty())
        || item
            .webhook_channel_ids
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != item.webhook_channel_ids.len()
        || item.ladder_index as usize > item.webhook_channel_ids.len()
        || (item.tier1_eligible && item.webhook_channel_ids.is_empty())
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation immutable webhook-plan invariant failed: escalation_id={} tier1_eligible={} ladder_index={} plan={:?}",
                item.escalation_id,
                item.tier1_eligible,
                item.ladder_index,
                item.webhook_channel_ids
            ),
        ));
    }
    let ack_fields_present = item.acked_at_unix_ms.is_some() && item.acked_via.is_some();
    let ack_fields_paired = item.acked_at_unix_ms.is_some() == item.acked_via.is_some();
    if !ack_fields_paired
        || (item.status == EscalationStatus::Acked && !ack_fields_present)
        || (matches!(
            item.status,
            EscalationStatus::Pending | EscalationStatus::Expired
        ) && ack_fields_present)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation acknowledgment fields disagree with status: escalation_id={} status={} acked_at={:?} acked_via={:?}",
                item.escalation_id,
                item.status.as_str(),
                item.acked_at_unix_ms,
                item.acked_via
            ),
        ));
    }
    validate_tier0_delivery(item)?;
    let mut identities = BTreeSet::new();
    for attempt in &item.channel_attempts {
        if attempt.outcome == WebhookAttemptOutcome::LegacyUnclassified {
            if !attempt.delivery_id.is_empty() {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "legacy webhook attempt unexpectedly has a durable delivery ID: escalation_id={} delivery_id={}",
                        item.escalation_id, attempt.delivery_id
                    ),
                ));
            }
            continue;
        }
        let expected_channel_id = item
            .webhook_channel_ids
            .get(attempt.ladder_index as usize)
            .map(String::as_str);
        let expected_delivery_id = expected_channel_id
            .map(|channel_id| webhook_delivery_id(&item.escalation_id, channel_id));
        let accepted = attempt.outcome == WebhookAttemptOutcome::Accepted;
        if expected_channel_id != Some(attempt.channel_id.as_str())
            || expected_delivery_id.as_deref() != Some(attempt.delivery_id.as_str())
            || attempt.attempt_number == 0
            || attempt.attempt_number > WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL
            || attempt.ladder_index > item.ladder_index
            || attempt.channel_name.is_empty()
            || attempt.url_host.is_empty()
            || accepted != attempt.ok
            || (accepted
                && (attempt
                    .http_status
                    .is_none_or(|status| !(200..300).contains(&status))
                    || attempt.error.is_some()))
            || (!accepted && attempt.error.is_none())
            || !identities.insert((attempt.delivery_id.clone(), attempt.attempt_number))
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook channel attempt invariant failed: escalation_id={} attempt={attempt:?} item_ladder_index={}",
                    item.escalation_id, item.ladder_index
                ),
            ));
        }
    }
    Ok(())
}

fn encode_item(item: &EscalationItem) -> Result<Vec<u8>, ErrorData> {
    validate_item(item)?;
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

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransitionGeneration {
    pub(crate) journal_ts_ns: u64,
    pub(crate) journal_seq: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TransitionProjectionJournalWitness {
    pub(crate) generation: TransitionGeneration,
    pub(crate) journal_key: Vec<u8>,
    pub(crate) journal_value: Vec<u8>,
    pub(crate) watermark_revision_sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionProjectionInput {
    generation: TransitionGeneration,
    journal_key_hex: String,
    journal_value_sha256: String,
    /// Exact canonical journal bytes, hex encoded. The cursor and journal row
    /// are born in one Calyx WAL commit, so this witness keeps a Pending cursor
    /// replayable after the 30-day journal retention horizon without treating
    /// a retired source row as corruption.
    journal_value_hex: String,
    anchor: String,
    spawn_id: Option<String>,
    session_id: Option<String>,
    state_from: String,
    state_to: String,
    reason_code: String,
    waiting_for: Option<String>,
    runaway: bool,
    evidence: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransitionProjectionPhase {
    Pending,
    Applied,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TransitionProjectionApplication {
    Escalation {
        escalation_id: String,
        approval_id: String,
    },
    NoEscalation {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionProjectionAppliedEvidence {
    event_id: String,
    at_unix_ms: u64,
    audit_value_sha256: String,
    application: TransitionProjectionApplication,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionProjectionAuditRecord {
    schema_version: u32,
    cursor_version: u32,
    event: String,
    event_id: String,
    anchor: String,
    generation: TransitionGeneration,
    state_to: String,
    reason_code: String,
    journal_value_sha256: String,
    at_unix_ms: u64,
    application: TransitionProjectionApplication,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionProjectionWatermark {
    schema_version: u32,
    cursor_version: u32,
    anchor: String,
    observed: TransitionProjectionInput,
    last_applied_generation: Option<TransitionGeneration>,
    phase: TransitionProjectionPhase,
    applied_evidence: Option<TransitionProjectionAppliedEvidence>,
    updated_at_unix_ms: u64,
}

/// Delta-first work index for exactly one Pending projection cursor.
///
/// The body binds both the complete cursor bytes and the predecessor physical
/// revisions used by the atomic publication CAS. Runtime reconciliation then
/// guards the current physical revisions of both rows before any projection
/// mutation. The index is therefore durable work state, not a best-effort
/// cache that may be rebuilt or silently skipped.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingTransitionProjectionIndex {
    schema_version: u32,
    index_version: u32,
    anchor: String,
    generation: TransitionGeneration,
    journal_key_hex: String,
    journal_value_sha256: String,
    cursor_key_hex: String,
    cursor_value_sha256: String,
    cursor_predecessor_revision_sha256: Option<String>,
    index_predecessor_revision_sha256: Option<String>,
    updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingProjectionIndexMigrationRecord {
    schema_version: u32,
    index_version: u32,
    migration_version: u32,
    completed_at_unix_ms: u64,
    cursor_rows_audited: u64,
    pending_rows_indexed: u64,
    applied_rows_verified: u64,
    audit_sha256: String,
}

#[derive(Clone, Debug)]
struct PendingProjectionIndexMigrationReadback {
    record: PendingProjectionIndexMigrationRecord,
    value: Vec<u8>,
    revision_sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecentIndexMigrationRecord {
    schema_version: u32,
    migration_version: u32,
    completed_at_unix_ms: u64,
    indexed_rows: u64,
    audit_sha256: String,
}

#[derive(Clone, Debug)]
struct RevisionedTransitionProjectionWatermark {
    record: TransitionProjectionWatermark,
    value: Vec<u8>,
    revision_sha256: [u8; 32],
}

#[derive(Clone, Debug)]
struct RevisionedPendingTransitionProjectionIndex {
    record: PendingTransitionProjectionIndex,
    value: Vec<u8>,
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
    if let ItemWriteGuard::Revision(expected_revision_sha256) = guard
        && let Some(current) = db
            .get_cf_revisioned(cf::CF_KV, &item_key)
            .map_err(storage_error)?
        && current.revision_sha256 == expected_revision_sha256
    {
        let current_value = live_revisioned_value(
            &current,
            &format!("escalation monotonic-time guard {}", item.escalation_id),
        )?;
        let current_item = decode_item_identity(&item.escalation_id, current_value)?;
        if item.updated_at_unix_ms < current_item.updated_at_unix_ms {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "revision-guarded escalation mutation would move durable time backward: escalation_id={} event={} current_updated_at_unix_ms={} proposed_updated_at_unix_ms={}; use a fresh mutation-boundary clock clamped to current state",
                    item.escalation_id,
                    event,
                    current_item.updated_at_unix_ms,
                    item.updated_at_unix_ms
                ),
            ));
        }
    }
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
        recent_index_row(item),
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
            expected_present = conflict.expected_revision_sha256.is_some(),
            actual_present = conflict.actual_revision_sha256.is_some(),
            "multi-key revision-guarded escalation mutation was not applied; caller must reread authoritative state (revision digests are intentionally not logged because a guarded policy may contain secrets)"
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
    let (recent_key, recent_value) = recent_index_row(item);
    let recent_readback = read_exact_row(db, &recent_key)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation recent index absent immediately after write: escalation_id={} key={}",
                item.escalation_id,
                hex_bytes(&recent_key)
            ),
        )
    })?;
    if recent_readback != recent_value {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation recent index bytes differed immediately after write: escalation_id={} key={} committed_seq={}",
                item.escalation_id,
                hex_bytes(&recent_key),
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
                validate_guarded_extra_row_identity(item, key, readback_value)?;
                tracing::info!(
                    code = "ESCALATION_GUARDED_EXTRA_READBACK_SUPERSEDED",
                    escalation_id = %item.escalation_id,
                    event,
                    committed_seq = outcome.committed_seq,
                    key = %hex_bytes(key),
                    "separate latest readback decoded and identity-validated a newer guarded extra-row revision; revision digests intentionally omitted"
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
    for (guard_index, revision_guard) in revision_guards.iter().enumerate() {
        if revision_guard.key == item_key
            || extra_rows
                .iter()
                .any(|(row_key, _)| row_key == &revision_guard.key)
        {
            continue;
        }
        let committed_revision = outcome.committed_revisions_sha256[guard_index];
        let readback = db
            .get_cf_revisioned(cf::CF_KV, &revision_guard.key)
            .map_err(storage_error)?;
        match (committed_revision, readback) {
            (None, None) => {}
            (Some(_), None) => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "guard-only Source of Truth disappeared after commit: escalation_id={} event={} guard_index={} key={} committed_seq={}",
                        item.escalation_id,
                        event,
                        guard_index,
                        hex_bytes(&revision_guard.key),
                        outcome.committed_seq
                    ),
                ));
            }
            (committed_revision, Some(readback)) => {
                let readback_value = live_revisioned_value(
                    &readback,
                    &format!(
                        "guard-only escalation row {} for {}",
                        hex_bytes(&revision_guard.key),
                        item.escalation_id
                    ),
                )?;
                validate_guarded_extra_row_identity(item, &revision_guard.key, readback_value)?;
                tracing::info!(
                    code = "ESCALATION_GUARD_ONLY_READBACK_VERIFIED",
                    escalation_id = %item.escalation_id,
                    event,
                    guard_index,
                    key = %hex_bytes(&revision_guard.key),
                    committed_seq = outcome.committed_seq,
                    superseded = committed_revision != Some(readback.revision_sha256),
                    "separate point read decoded and identity-validated the guard-only Source of Truth; revision digests intentionally omitted"
                );
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

fn validate_guarded_extra_row_identity(
    item: &EscalationItem,
    key: &[u8],
    value: &[u8],
) -> Result<(), ErrorData> {
    if key == CONFIG_KEY.as_bytes() {
        let policy = decode_json::<EscalationPolicy>(value).map_err(|error| {
            mcp_error(
                error.code(),
                format!("escalation policy decode failed during guarded claim readback: {error}"),
            )
        })?;
        return validate_policy(&policy, true, error_codes::STORAGE_CORRUPTED);
    }
    if key.starts_with(OUTBOX_PREFIX.as_bytes()) {
        let record = decode_json::<WebhookOutboxRecord>(value).map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "webhook outbox decode failed during revision readback: key={} error={error}",
                    hex_bytes(key)
                ),
            )
        })?;
        validate_outbox_record(key, &record)?;
        if record.escalation_id != item.escalation_id {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook outbox/escalation identity mismatch: item_escalation_id={:?} outbox_escalation_id={:?} delivery_id={:?}",
                    item.escalation_id, record.escalation_id, record.delivery_id
                ),
            ));
        }
        return Ok(());
    }
    if key.starts_with(PROJECTION_WATERMARK_PREFIX.as_bytes()) {
        let record = decode_json::<TransitionProjectionWatermark>(value).map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "transition projection watermark decode failed during revision readback: key={} error={error}",
                    hex_bytes(key)
                ),
            )
        })?;
        validate_projection_watermark(key, &record)?;
        if record.anchor != item.anchor {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "transition projection watermark/escalation identity mismatch: item_anchor={:?} watermark_anchor={:?}",
                    item.anchor, record.anchor
                ),
            ));
        }
        return Ok(());
    }
    if key.starts_with(PENDING_PROJECTION_INDEX_PREFIX.as_bytes()) {
        let record = decode_json::<PendingTransitionProjectionIndex>(value).map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "Pending transition projection index decode failed during revision readback: key={} error={error}",
                    hex_bytes(key)
                ),
            )
        })?;
        validate_pending_projection_index(key, &record)?;
        if record.anchor != item.anchor {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending transition projection index/escalation identity mismatch: item_anchor={:?} index_anchor={:?}",
                    item.anchor, record.anchor
                ),
            ));
        }
        return Ok(());
    }
    if key.starts_with(APPROVAL_ITEM_PREFIX.as_bytes()) {
        let approval = validate_linked_approval_item_identity(key, value)?;
        validate_linked_approval_for_escalation(item, &approval)?;
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
    for migration_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let Some(revisioned) = db
            .get_cf_revisioned(cf::CF_KV, &key)
            .map_err(storage_error)?
        else {
            return Ok(None);
        };
        let value =
            live_revisioned_value(&revisioned, &format!("escalation item {escalation_id}"))?;
        let item = decode_item_identity(escalation_id, value)?;
        let (mut item, migration_required) = prepare_legacy_item_migration(item);
        if !migration_required {
            validate_item(&item)?;
            return Ok(Some(RevisionedEscalationItem {
                item,
                revision_sha256: revisioned.revision_sha256,
            }));
        }
        item.updated_at_unix_ms = unix_time_ms_now().max(item.updated_at_unix_ms);
        validate_item(&item)?;
        match write_item_and_audit_if_revision(
            db,
            &item,
            "legacy_delivery_state_classified",
            json!({
                "reason": "legacy receiver or Tier-0 state required explicit classification before future side effects",
                "network_io": "refused",
            }),
            revisioned.revision_sha256,
        )? {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_ITEM_LEGACY_DELIVERY_STATE_MIGRATED",
                    escalation_id,
                    migration_attempt,
                    committed_seq,
                    "readback=CF_KV legacy item delivery state was classified before future side effects"
                );
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_ITEM_MIGRATION_REVISION_RETRY",
                    escalation_id,
                    migration_attempt,
                    observed_seq,
                    "legacy item migration raced another writer; rereading authoritative state"
                );
            }
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "legacy escalation item {escalation_id} could not acquire a stable migration revision after {ACK_REVISION_MAX_ATTEMPTS} attempts"
        ),
    ))
}

fn decode_item(escalation_id: &str, value: &[u8]) -> Result<EscalationItem, ErrorData> {
    let item = decode_item_identity(escalation_id, value)?;
    validate_item(&item)?;
    Ok(item)
}

fn decode_item_identity(escalation_id: &str, value: &[u8]) -> Result<EscalationItem, ErrorData> {
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

fn prepare_legacy_item_migration(mut item: EscalationItem) -> (EscalationItem, bool) {
    let mut changed = false;
    let mut webhook_changed = false;
    if item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified {
        if let Some(reason) = item
            .tier0_suppressed_reason
            .clone()
            .filter(|_| !item.tier0_fired && item.tier0_toast_removed.is_none())
        {
            item.tier0_delivery = Tier0ToastDelivery::Suppressed {
                reason,
                at_unix_ms: item.updated_at_unix_ms,
            };
            changed = true;
        }
    }
    for attempt in &mut item.channel_attempts {
        if attempt.channel_id.is_empty()
            && (attempt.outcome != WebhookAttemptOutcome::LegacyUnclassified
                || !attempt.delivery_id.is_empty())
        {
            attempt.outcome = WebhookAttemptOutcome::LegacyUnclassified;
            attempt.delivery_id.clear();
            changed = true;
            webhook_changed = true;
        }
    }
    if item.webhook_channel_ids.is_empty()
        && (item.tier1_eligible || item.ladder_index > 0 || !item.channel_attempts.is_empty())
    {
        changed = true;
        webhook_changed = true;
    }
    if webhook_changed {
        item.tier1_eligible = false;
        if item.webhook_channel_ids.is_empty() {
            item.ladder_index = 0;
        }
        item.next_escalate_at_unix_ms = None;
        item.tier1_suppressed_reason =
            Some("legacy_webhook_state_requires_receipt_v1_reconfiguration".to_owned());
    }
    (item, changed)
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
    lease: &mut synapse_storage::CoherentScanLease,
) -> Result<synapse_storage::FixedWidthScanPage, ErrorData> {
    db.scan_cf_fixed_width_range_page_coherent(lease, SCAN_CHUNK_ROWS)
        .map_err(storage_error)
}

fn finish_coherent_scan<T>(
    db: &Db,
    lease: &mut synapse_storage::CoherentScanLease,
    context: &'static str,
    scan_result: Result<T, ErrorData>,
) -> Result<T, ErrorData> {
    let lease_id = lease.lease_id;
    let snapshot_seq = lease.snapshot_seq;
    let release_result = db.release_coherent_scan(lease);
    match (scan_result, release_result) {
        (Err(scan_error), Ok(_)) => Err(scan_error),
        (Err(scan_error), Err(release_error)) => Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "{context}: {}; additionally failed to release coherent lease_id={lease_id} \
                 snapshot_seq={snapshot_seq}: {release_error}",
                scan_error.message
            ),
        )),
        (Ok(_value), Err(error)) => Err(mcp_error(
            error.code(),
            format!(
                "{context}: coherent snapshot release failed lease_id={lease_id} \
                 snapshot_seq={snapshot_seq}: {error}"
            ),
        )),
        (Ok(_), Ok(false)) => Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "{context}: coherent snapshot expired before release lease_id={lease_id} \
                 snapshot_seq={snapshot_seq}; repeat the bounded one-generation operation"
            ),
        )),
        (Ok(value), Ok(true)) => Ok(value),
    }
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

fn decode_item_page(
    db: &Db,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Vec<EscalationItemRow>, ErrorData> {
    let mut decoded = Vec::with_capacity(rows.len());
    for (key, _scanned_value) in rows {
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
        // The page is a candidate snapshot, not the verdict. Point-read each
        // candidate so legacy rows are CAS-migrated and concurrent deletion or
        // supersession is observed at the authoritative key.
        if let Some(revisioned) = read_item_revisioned(db, id)? {
            decoded.push(EscalationItemRow {
                key,
                item: revisioned.item,
            });
        }
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
    let mut lease = db
        .pin_cf_fixed_width_range_scan(
            cf::CF_KV,
            &start,
            &end,
            synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS,
        )
        .map_err(storage_error)?;
    let lease_id = lease.lease_id;
    let snapshot_seq = lease.snapshot_seq;
    let mut snapshot_telemetry = ItemPageSnapshotTelemetry::default();
    let mut out = Vec::new();
    let mut pages = 0usize;
    let scan_result = (|| -> Result<(), ErrorData> {
        loop {
            let cursor_before = lease.next_after().map(ToOwned::to_owned);
            let page = scan_item_page(db, &mut lease)?;
            pages = pages.saturating_add(1);
            snapshot_telemetry.observe(page.snapshot_seq)?;
            let next = next_item_page_cursor(&page, cursor_before.as_deref())?;
            out.extend(decode_item_page(db, page.rows)?);
            if next.is_none() {
                break;
            }
        }
        Ok(())
    })();
    finish_coherent_scan(db, &mut lease, "ESCALATION_ITEM_SCAN", scan_result)?;
    tracing::debug!(
        code = "ESCALATION_ITEM_SCAN_PAGE_SNAPSHOTS",
        scan_mode = "synchronous",
        scan_pages = pages,
        first_snapshot_seq = snapshot_telemetry.first,
        last_snapshot_seq = snapshot_telemetry.last,
        snapshot_seq_changes = snapshot_telemetry.changes,
        lease_id,
        snapshot_seq,
        "completed ordered escalation item paging at one pinned Calyx generation"
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
    let mut lease = db
        .pin_cf_fixed_width_range_scan(
            cf::CF_KV,
            &start,
            &end,
            synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS,
        )
        .map_err(storage_error)?;
    let lease_id = lease.lease_id;
    let snapshot_seq = lease.snapshot_seq;
    let mut snapshot_telemetry = ItemPageSnapshotTelemetry::default();
    let mut rows = Vec::new();
    let mut pages = 0usize;
    let mut candidate_rows_examined = 0usize;
    let mut expired_rows_skipped = 0usize;
    let scan_result = async {
        loop {
            if shutdown.is_cancelled() {
                return Ok(None);
            }
            let cursor_before = lease.next_after().map(ToOwned::to_owned);
            let page_started = Instant::now();
            let page = scan_item_page(db, &mut lease)?;
            pages = pages.saturating_add(1);
            candidate_rows_examined =
                candidate_rows_examined.saturating_add(page.candidate_rows_examined);
            expired_rows_skipped = expired_rows_skipped.saturating_add(page.expired_rows_skipped);
            snapshot_telemetry.observe(page.snapshot_seq)?;
            let next = next_item_page_cursor(&page, cursor_before.as_deref())?;
            rows.extend(decode_item_page(db, page.rows)?);
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
            if next.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(Some(()))
    }
    .await;
    let Some(()) = finish_coherent_scan(
        db,
        &mut lease,
        "ESCALATION_ITEM_SCAN_CANCELLABLE",
        scan_result,
    )?
    else {
        return Ok(None);
    };
    tracing::debug!(
        code = "ESCALATION_ITEM_SCAN_PAGE_SNAPSHOTS",
        scan_mode = "cancellable",
        scan_pages = pages,
        first_snapshot_seq = snapshot_telemetry.first,
        last_snapshot_seq = snapshot_telemetry.last,
        snapshot_seq_changes = snapshot_telemetry.changes,
        lease_id,
        snapshot_seq,
        "completed ordered escalation item paging at one pinned Calyx generation"
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
    created_at_unix_ms: u64,
    scanned_updated_at_unix_ms: u64,
}

const fn outbox_state_is_retention_safe(state: WebhookOutboxState) -> bool {
    matches!(
        state,
        WebhookOutboxState::Accepted
            | WebhookOutboxState::UnknownTerminal
            | WebhookOutboxState::RetryExhausted
            | WebhookOutboxState::Abandoned
            | WebhookOutboxState::TerminalFailure
    )
}

const fn tier0_state_is_retention_safe(state: &Tier0ToastDelivery) -> bool {
    matches!(
        state,
        Tier0ToastDelivery::Removed { .. } | Tier0ToastDelivery::Suppressed { .. }
    )
}

fn first_unreconciled_terminal_outbox(
    db: &Db,
    escalation_id: &str,
) -> Result<Option<WebhookOutboxRecord>, ErrorData> {
    for ladder_index in 0..MAX_WEBHOOKS as u32 {
        let Some(outbox) = read_outbox_revisioned(db, escalation_id, ladder_index)? else {
            continue;
        };
        if !outbox_state_is_retention_safe(outbox.record.state) {
            return Ok(Some(outbox.record));
        }
    }
    Ok(None)
}

fn delete_terminal_outboxes(db: &Db, item: &EscalationItem) -> Result<usize, ErrorData> {
    if item.status.is_open() {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "refused to delete webhook outboxes for open escalation {}",
                item.escalation_id
            ),
        ));
    }
    for attempt in 1..=DELETE_REVISION_MAX_ATTEMPTS {
        let mut guards = Vec::new();
        let mut keys = Vec::new();
        for ladder_index in 0..MAX_WEBHOOKS as u32 {
            if let Some(outbox) = read_outbox_revisioned(db, &item.escalation_id, ladder_index)? {
                if !outbox_state_is_retention_safe(outbox.record.state) {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "refused to prune webhook truth that still requires reconciliation: escalation_id={} delivery_id={} ladder_index={} attempt_number={} state={:?}",
                            item.escalation_id,
                            outbox.record.delivery_id,
                            ladder_index,
                            outbox.record.attempt_number,
                            outbox.record.state
                        ),
                    ));
                }
                let key = outbox_key(&item.escalation_id, ladder_index);
                guards.push(RevisionGuard::new(
                    key.clone(),
                    Some(outbox.revision_sha256),
                ));
                keys.push(key);
            }
        }
        if keys.is_empty() {
            return Ok(0);
        }
        let outcome = db
            .mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                guards,
                keys.iter().cloned(),
                std::iter::empty::<(Vec<u8>, Vec<u8>)>(),
            )
            .map_err(storage_error)?;
        if !outcome.applied {
            tracing::info!(
                code = "ESCALATION_OUTBOX_RETENTION_REVISION_RETRY",
                escalation_id = %item.escalation_id,
                attempt,
                observed_seq = outcome.committed_seq,
                outbox_rows = keys.len(),
                "terminal outbox retention lost a revision race; rereading every outbox"
            );
            continue;
        }
        if outcome.committed_revisions_sha256.len() != keys.len()
            || outcome
                .committed_revisions_sha256
                .iter()
                .any(Option::is_some)
        {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "terminal outbox delete returned invalid revision shape: escalation_id={} keys={} revisions={} non_absent={}",
                    item.escalation_id,
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
                        "terminal webhook outbox remained after guarded delete: escalation_id={} key={} committed_seq={}",
                        item.escalation_id,
                        hex_bytes(key),
                        outcome.committed_seq
                    ),
                ));
            }
        }
        tracing::info!(
            code = "ESCALATION_OUTBOX_RETENTION_PRUNED",
            escalation_id = %item.escalation_id,
            outbox_rows = keys.len(),
            committed_seq = outcome.committed_seq,
            "readback=CF_KV terminal webhook outbox bodies are absent before item deletion"
        );
        return Ok(keys.len());
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "terminal webhook outboxes for escalation {} could not acquire stable revisions after {DELETE_REVISION_MAX_ATTEMPTS} attempts",
            item.escalation_id
        ),
    ))
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
    if item
        .tier0_prepared_payload
        .as_ref()
        .is_some_and(legacy_prepared_toast_payload_v1_valid)
    {
        tracing::warn!(
            code = "ESCALATION_RETENTION_TIER0_PAYLOAD_MIGRATION_REQUIRED",
            escalation_id = %item.escalation_id,
            "terminal escalation retention deferred until the schema-v1 frozen toast is rerendered, exact-compared, and durably bound to its logical request"
        );
        return Ok(None);
    }
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
    if !tier0_state_is_retention_safe(&item.tier0_delivery) {
        tracing::warn!(
            code = "ESCALATION_RETENTION_TIER0_RECONCILIATION_REQUIRED",
            escalation_id = %item.escalation_id,
            state = ?item.tier0_delivery,
            "terminal escalation retention deferred until Action Center Tag+Group absence is physically proved and durably recorded"
        );
        return Ok(None);
    }
    if let Some(outbox) = first_unreconciled_terminal_outbox(db, &item.escalation_id)? {
        tracing::warn!(
            code = "ESCALATION_RETENTION_OUTBOX_RECONCILIATION_REQUIRED",
            escalation_id = %item.escalation_id,
            delivery_id = %outbox.delivery_id,
            ladder_index = outbox.ladder_index,
            attempt_number = outbox.attempt_number,
            state = ?outbox.state,
            "terminal escalation retention deferred; the physical webhook outbox remains authoritative until the worker reconciles its delivery state"
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
    let _deleted_outboxes = delete_terminal_outboxes(db, &item)?;
    let latest = db
        .get_cf_revisioned(cf::CF_KV, &candidate.key)
        .map_err(storage_error)?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "terminal escalation {} disappeared while pruning linked outboxes",
                    candidate.escalation_id
                ),
            )
        })?;
    let latest_item = decode_item(
        &candidate.escalation_id,
        live_revisioned_value(
            &latest,
            &format!("terminal escalation item {}", candidate.escalation_id),
        )?,
    )?;
    if latest_item.status.is_open()
        || latest_item.updated_at_unix_ms != candidate.scanned_updated_at_unix_ms
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "terminal escalation changed while its outboxes were pruned: escalation_id={} status={} scanned_updated_at={} latest_updated_at={}",
                candidate.escalation_id,
                latest_item.status.as_str(),
                candidate.scanned_updated_at_unix_ms,
                latest_item.updated_at_unix_ms
            ),
        ));
    }
    Ok(Some(RevisionGuard::new(
        candidate.key.clone(),
        Some(latest.revision_sha256),
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
        let mut guards = Vec::with_capacity(pending.len() * 2);
        let mut keys = Vec::with_capacity(pending.len() * 2);
        for (candidate, item_guard) in &pending {
            let recent_key =
                recent_index_key_parts(candidate.created_at_unix_ms, &candidate.escalation_id);
            let recent = db
                .get_cf_revisioned(cf::CF_KV, &recent_key)
                .map_err(storage_error)?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "terminal escalation recent index is absent before atomic retention delete: escalation_id={} key={}",
                            candidate.escalation_id,
                            hex_bytes(&recent_key)
                        ),
                    )
                })?;
            let recent_value = live_revisioned_value(
                &recent,
                &format!(
                    "terminal escalation recent index {}",
                    candidate.escalation_id
                ),
            )?;
            if recent_value != candidate.escalation_id.as_bytes() {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "terminal escalation recent index identity mismatch before retention delete: escalation_id={} key={}",
                        candidate.escalation_id,
                        hex_bytes(&recent_key)
                    ),
                ));
            }
            guards.push(item_guard.clone());
            guards.push(RevisionGuard::new(
                recent_key.clone(),
                Some(recent.revision_sha256),
            ));
            keys.push(candidate.key.clone());
            keys.push(recent_key);
        }
        let deleted_items = pending.len();
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
            return Ok(Some(deleted_items));
        }

        let conflict = outcome.conflict.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "{context} returned applied=false without revision conflict detail at attempt {attempt}"
                ),
            )
        })?;
        let conflict_candidate_index = conflict.guard_index / 2;
        let expected_conflict_key = pending.get(conflict_candidate_index).map(|(candidate, _)| {
            if conflict.guard_index % 2 == 0 {
                candidate.key.clone()
            } else {
                recent_index_key_parts(candidate.created_at_unix_ms, &candidate.escalation_id)
            }
        });
        if expected_conflict_key.as_deref() != Some(conflict.key.as_slice()) {
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
        let (candidate, _stale_guard) = pending.remove(conflict_candidate_index);
        if let Some(refreshed_guard) = current_terminal_delete_guard(db, &candidate)? {
            pending.insert(conflict_candidate_index, (candidate, refreshed_guard));
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
        .filter(|row| {
            !row.item.status.is_open()
                && matches!(
                    row.item.tier0_delivery,
                    Tier0ToastDelivery::Removed { .. } | Tier0ToastDelivery::Suppressed { .. }
                )
        })
        .map(|row| {
            (
                row.item.updated_at_unix_ms,
                TerminalItemDeleteCandidate {
                    key: row.key.clone(),
                    escalation_id: row.item.escalation_id.clone(),
                    created_at_unix_ms: row.item.created_at_unix_ms,
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

/// All escalation items. Large queues are scanned in bounded storage windows;
/// terminal item rows are compacted by the delivery sweep instead of making
/// the queue fail closed at the historical row limit.
fn scan_items(db: &Db) -> Result<Vec<EscalationItem>, ErrorData> {
    scan_item_rows(db).map(|rows| rows.into_iter().map(|row| row.item).collect())
}

fn fixed_prefix_bounds(prefix: &str, key_len: usize) -> (Vec<u8>, Vec<u8>) {
    let mut start = prefix.as_bytes().to_vec();
    start.resize(key_len, 0);
    let mut end = prefix.as_bytes().to_vec();
    for byte in end.iter_mut().rev() {
        if *byte != u8::MAX {
            *byte += 1;
            end.resize(key_len, 0);
            return (start, end);
        }
        *byte = 0;
    }
    unreachable!("ASCII storage prefix always has a lexicographic successor")
}

fn read_recent_index_migration(db: &Db) -> Result<Option<RecentIndexMigrationRecord>, ErrorData> {
    let Some(value) = read_exact_row(db, RECENT_INDEX_MIGRATION_KEY.as_bytes())? else {
        return Ok(None);
    };
    let record = decode_json::<RecentIndexMigrationRecord>(&value).map_err(|error| {
        mcp_error(
            error.code(),
            format!("recent escalation index migration sentinel decode failed: {error}"),
        )
    })?;
    if record.schema_version != SCHEMA_VERSION
        || record.migration_version != 1
        || record.completed_at_unix_ms == 0
        || !is_lower_hex_exact(&record.audit_sha256, 64)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("recent escalation index migration sentinel invariant failed: {record:?}"),
        ));
    }
    Ok(Some(record))
}

fn ensure_recent_index_migration_locked(db: &Db) -> Result<(), ErrorData> {
    if read_recent_index_migration(db)?.is_some() {
        return Ok(());
    }
    let items = scan_items(db)?;
    let mut audit = Sha256::new();
    audit.update(b"synapse.escalation.recent-index-migration.v1\0");
    for item in &items {
        let (key, value) = recent_index_row(item);
        match read_exact_row(db, &key)? {
            Some(existing) if existing == value => {}
            Some(existing) => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "recent escalation index conflicts with authoritative item: escalation_id={} key={} expected_sha256={} actual_sha256={}",
                        item.escalation_id,
                        hex_bytes(&key),
                        hex_bytes(&Sha256::digest(&value)),
                        hex_bytes(&Sha256::digest(&existing))
                    ),
                ));
            }
            None => {
                let outcome = db
                    .put_batch_if_revision_pressure_bypass(
                        cf::CF_KV,
                        &key,
                        None,
                        [(key.clone(), value.clone())],
                    )
                    .map_err(storage_error)?;
                if !outcome.applied {
                    return Err(mcp_error(
                        error_codes::STORAGE_WRITE_FAILED,
                        format!(
                            "recent escalation index migration raced another writer: escalation_id={} key={} observed_seq={}",
                            item.escalation_id,
                            hex_bytes(&key),
                            outcome.committed_seq
                        ),
                    ));
                }
                let readback = read_exact_row(db, &key)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "recent escalation index disappeared after migration: key={}",
                            hex_bytes(&key)
                        ),
                    )
                })?;
                if readback != value {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "recent escalation index readback differed after migration: key={}",
                            hex_bytes(&key)
                        ),
                    ));
                }
            }
        }
        audit.update((key.len() as u64).to_be_bytes());
        audit.update(&key);
        audit.update((value.len() as u64).to_be_bytes());
        audit.update(&value);
    }
    let record = RecentIndexMigrationRecord {
        schema_version: SCHEMA_VERSION,
        migration_version: 1,
        completed_at_unix_ms: unix_time_ms_now(),
        indexed_rows: items.len() as u64,
        audit_sha256: hex_bytes(&audit.finalize()),
    };
    let value = encode_json(&record).map_err(|error| {
        mcp_error(
            error.code(),
            format!("recent escalation index migration sentinel encode failed: {error}"),
        )
    })?;
    let key = RECENT_INDEX_MIGRATION_KEY.as_bytes();
    let outcome = db
        .put_batch_if_revision_pressure_bypass(cf::CF_KV, key, None, [(key.to_vec(), value)])
        .map_err(storage_error)?;
    if !outcome.applied || read_recent_index_migration(db)?.as_ref() != Some(&record) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            "recent escalation index migration sentinel did not pass independent readback",
        ));
    }
    tracing::info!(
        code = "ESCALATION_RECENT_INDEX_MIGRATION_COMPLETED",
        indexed_rows = record.indexed_rows,
        audit_sha256 = %record.audit_sha256,
        committed_seq = outcome.committed_seq,
        "durable newest-first escalation index was built and independently read back"
    );
    Ok(())
}

fn ensure_recent_index_migration(db: &Db) -> Result<(), ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| ensure_recent_index_migration_locked(db))
        .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("recent escalation index migration lock failed: {detail}"),
        )
    })?
}

fn count_open_index_rows(db: &Db) -> Result<usize, ErrorData> {
    let (start, end) = fixed_prefix_bounds(OPEN_INDEX_PREFIX, OPEN_INDEX_KEY_LEN);
    let mut lease = db
        .pin_cf_fixed_width_range_scan(
            cf::CF_KV,
            &start,
            &end,
            synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS,
        )
        .map_err(storage_error)?;
    let mut total = 0usize;
    let scan_result = (|| -> Result<(), ErrorData> {
        loop {
            let page = db
                .scan_cf_fixed_width_range_page_coherent(&mut lease, LIST_INDEX_PAGE_ROWS)
                .map_err(storage_error)?;
            for (key, value) in page.rows {
                if key.len() != OPEN_INDEX_KEY_LEN || !key.starts_with(OPEN_INDEX_PREFIX.as_bytes())
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "open escalation index returned malformed key: key={}",
                            hex_bytes(&key)
                        ),
                    ));
                }
                let record = decode_json::<OpenEscalationIndexRecord>(&value).map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "open escalation index decode failed: key={} error={error}",
                            hex_bytes(&key)
                        ),
                    )
                })?;
                validate_open_index_record(&key, &record)?;
                total = total.saturating_add(usize::from(record.is_open));
            }
            if !page.more {
                break;
            }
        }
        Ok(())
    })();
    finish_coherent_scan(db, &mut lease, "ESCALATION_OPEN_INDEX_COUNT", scan_result)?;
    Ok(total)
}

fn list_recent_items(
    db: &Db,
    status: Option<EscalationStatus>,
    anchor: Option<&str>,
    limit: usize,
) -> Result<Vec<EscalationItem>, ErrorData> {
    ensure_recent_index_migration(db)?;
    let (start, end) = fixed_prefix_bounds(RECENT_INDEX_PREFIX, RECENT_INDEX_KEY_LEN);
    let mut lease = db
        .pin_cf_fixed_width_range_scan(
            cf::CF_KV,
            &start,
            &end,
            synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS,
        )
        .map_err(storage_error)?;
    let mut out = Vec::with_capacity(limit);
    let scan_result = (|| -> Result<(), ErrorData> {
        while out.len() < limit {
            let page = db
                .scan_cf_fixed_width_range_page_coherent(
                    &mut lease,
                    LIST_INDEX_PAGE_ROWS.min(limit.max(1)),
                )
                .map_err(storage_error)?;
            for (key, value) in page.rows {
                if key.len() != RECENT_INDEX_KEY_LEN
                    || !key.starts_with(RECENT_INDEX_PREFIX.as_bytes())
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "recent escalation index returned malformed key: key={}",
                            hex_bytes(&key)
                        ),
                    ));
                }
                let id = std::str::from_utf8(&value).map_err(|error| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "recent escalation index value is not UTF-8: key={} error={error}",
                            hex_bytes(&key)
                        ),
                    )
                })?;
                validate_escalation_id(id)?;
                let item = read_item(db, id)?.ok_or_else(|| {
                    mcp_error(error_codes::STORAGE_CORRUPTED, format!("recent escalation index points to missing item: escalation_id={id} key={}", hex_bytes(&key)))
                })?;
                if recent_index_key(&item) != key {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "recent escalation index key is not bound to authoritative item: escalation_id={id} key={}",
                            hex_bytes(&key)
                        ),
                    ));
                }
                if status.is_none_or(|expected| expected == item.status)
                    && anchor.is_none_or(|expected| expected == item.anchor)
                {
                    out.push(item);
                    if out.len() == limit {
                        break;
                    }
                }
            }
            if !page.more {
                break;
            }
        }
        Ok(())
    })();
    finish_coherent_scan(db, &mut lease, "ESCALATION_RECENT_INDEX_LIST", scan_result)?;
    Ok(out)
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

fn retained_tier0_toast_tags(db: &Db) -> Result<Vec<String>, ErrorData> {
    Ok(scan_items(db)?
        .into_iter()
        .filter(|item| {
            !matches!(
                item.tier0_delivery,
                Tier0ToastDelivery::Removed { .. } | Tier0ToastDelivery::Suppressed { .. }
            )
        })
        .filter_map(|item| {
            let tag = tier0_delivery_tag(&item.tier0_delivery)
                .map_or_else(|| escalation_toast_tag(&item.escalation_id), str::to_owned);
            (tier0_group_for_exact_tag(&item, &tag) == Some(SYNAPSE_ESCALATION_TOAST_GROUP))
                .then_some(tag)
        })
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
    db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.clone(), value.clone())])
        .map_err(storage_error)?;
    let readback = read_exact_row(db, &row_key)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "orphan toast cleanup audit row absent immediately after write for key {}",
                String::from_utf8_lossy(&row_key)
            ),
        )
    })?;
    if readback != value {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "orphan toast cleanup audit row readback differed byte-for-byte for key {}: expected_bytes={} actual_bytes={}",
                String::from_utf8_lossy(&row_key),
                value.len(),
                readback.len()
            ),
        ));
    }
    Ok(Some(hex_encode_bytes(&row_key)))
}

// ---------------------------------------------------------------------------
// Policy storage
// ---------------------------------------------------------------------------

fn validate_webhook_channel(
    channel: &WebhookChannel,
    allow_legacy_contract: bool,
    code: &'static str,
) -> Result<(), ErrorData> {
    let legacy_contract =
        channel.idempotency_contract != WebhookIdempotencyContract::SynapseReceiptV1;
    let legacy_missing_id =
        allow_legacy_contract && legacy_contract && channel.channel_id.is_empty();
    if !legacy_missing_id
        && (channel.channel_id.trim().is_empty()
            || channel.channel_id.chars().count() > MAX_NAME_CHARS
            || !channel
                .channel_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    {
        return Err(mcp_error(
            code,
            format!(
                "webhook channel_id must be 1..={MAX_NAME_CHARS} ASCII letters/digits/dot/dash/underscore"
            ),
        ));
    }
    if channel.name.trim().is_empty() || channel.name.chars().count() > MAX_NAME_CHARS {
        return Err(mcp_error(
            code,
            format!("webhook name must be 1..={MAX_NAME_CHARS} chars"),
        ));
    }
    if channel.url.is_empty() || channel.url.chars().count() > MAX_URL_CHARS {
        return Err(mcp_error(
            code,
            format!("webhook url must be 1..={MAX_URL_CHARS} chars"),
        ));
    }
    let url = reqwest::Url::parse(&channel.url).map_err(|error| {
        mcp_error(
            code,
            format!(
                "webhook url for '{}' is not a valid URL: {error}",
                channel.name
            ),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(mcp_error(
            code,
            format!(
                "webhook url for '{}' must be an absolute http:// or https:// URL with a host",
                channel.name
            ),
        ));
    }
    if !legacy_contract
        && (!url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some())
    {
        return Err(mcp_error(
            code,
            format!(
                "webhook url for '{}' must not contain userinfo, query credentials, or a fragment; use the dedicated secret field and an exact request target",
                channel.name
            ),
        ));
    }
    if !legacy_contract && url.scheme() != "https" {
        return Err(mcp_error(
            code,
            format!(
                "webhook '{}' uses synapse_receipt_v1 and must use HTTPS so readiness and durable receipts are authenticated",
                channel.name
            ),
        ));
    }
    if !legacy_contract && url.as_str() != channel.url {
        return Err(mcp_error(
            code,
            format!(
                "webhook '{}' URL is not in the canonical form used for receiver identity and signatures; provide the URL parser canonical form exactly",
                channel.name
            ),
        ));
    }
    if channel.idempotency_contract != WebhookIdempotencyContract::SynapseReceiptV1
        && !allow_legacy_contract
    {
        return Err(mcp_error(
            code,
            format!(
                "webhook '{}' must explicitly set idempotency_contract=synapse_receipt_v1; unsupported_legacy and synapse_echo_v1 are read-only migration states",
                channel.name
            ),
        ));
    }
    if let Some(secret) = &channel.secret {
        let invalid_secret = if legacy_contract {
            secret.chars().count() > MAX_SECRET_CHARS
        } else {
            !(MIN_SECRET_CHARS..=MAX_SECRET_CHARS).contains(&secret.len())
        };
        if invalid_secret {
            return Err(mcp_error(
                code,
                format!(
                    "webhook secret for '{}' must be {MIN_SECRET_CHARS}..={MAX_SECRET_CHARS} UTF-8 bytes when present",
                    channel.name
                ),
            ));
        }
    }
    Ok(())
}

fn validate_policy(
    policy: &EscalationPolicy,
    allow_legacy_contract: bool,
    code: &'static str,
) -> Result<(), ErrorData> {
    if policy.schema_version != SCHEMA_VERSION {
        return Err(mcp_error(
            code,
            format!(
                "escalation policy schema_version={} instead of {}",
                policy.schema_version, SCHEMA_VERSION
            ),
        ));
    }
    if policy.webhooks.len() > MAX_WEBHOOKS {
        return Err(mcp_error(
            code,
            format!(
                "at most {MAX_WEBHOOKS} webhooks are allowed; got {}",
                policy.webhooks.len()
            ),
        ));
    }
    let mut names = BTreeSet::new();
    let mut channel_ids = BTreeSet::new();
    let mut fingerprints = BTreeSet::new();
    let duplicate_legacy_rows_allowed = allow_legacy_contract
        && policy.webhooks.iter().all(|channel| {
            channel.idempotency_contract != WebhookIdempotencyContract::SynapseReceiptV1
        });
    for channel in &policy.webhooks {
        validate_webhook_channel(channel, allow_legacy_contract, code)?;
        if !names.insert(channel.name.clone()) && !duplicate_legacy_rows_allowed {
            return Err(mcp_error(
                code,
                format!("duplicate webhook name {:?} is not allowed", channel.name),
            ));
        }
        if !channel_ids.insert(channel.channel_id.clone()) {
            return Err(mcp_error(
                code,
                format!(
                    "duplicate webhook channel_id {:?} is not allowed",
                    channel.channel_id
                ),
            ));
        }
        let generation = policy
            .receiver_generations
            .get(&channel.channel_id)
            .ok_or_else(|| {
                mcp_error(
                    code,
                    format!(
                        "webhook channel {:?} has no opaque receiver generation",
                        channel.channel_id
                    ),
                )
            })?;
        if !valid_receiver_generation(generation) {
            return Err(mcp_error(
                code,
                format!(
                    "webhook channel {:?} has an invalid opaque receiver generation",
                    channel.channel_id
                ),
            ));
        }
        let fingerprint = webhook_receiver_fingerprint(channel);
        if !fingerprints.insert(fingerprint) && !duplicate_legacy_rows_allowed {
            return Err(mcp_error(
                code,
                format!(
                    "duplicate webhook receiver configuration for {:?} is not allowed",
                    channel.name
                ),
            ));
        }
    }
    if policy.receiver_generations.len() != channel_ids.len()
        || policy
            .receiver_generations
            .keys()
            .any(|channel_id| !channel_ids.contains(channel_id))
    {
        return Err(mcp_error(
            code,
            "escalation policy receiver generations do not exactly match the configured channel IDs",
        ));
    }
    if policy.ack_window_ms == 0
        || policy.critical_ack_window_ms == 0
        || policy.ttl_ordinary_ms == 0
        || policy.ttl_sensitive_ms == 0
    {
        return Err(mcp_error(
            code,
            "escalation ack windows and TTLs must all be >= 1",
        ));
    }
    if let Some(quiet) = policy.quiet_hours
        && (quiet.start_minute >= 1440 || quiet.end_minute >= 1440)
    {
        return Err(mcp_error(code, "quiet_hours minutes must be in 0..1440"));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct RevisionedEscalationPolicy {
    policy: EscalationPolicy,
    revision_sha256: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug)]
struct PolicyDecisionGuard {
    revision_sha256: Option<[u8; 32]>,
}

/// Upgrade pre-generation policies under CAS. Historical receiver rows are
/// deliberately disabled: they were admitted without the stable receiver
/// generation and strict receipt prerequisites required by the current
/// contract, so silently treating them as sendable would invent safety.
fn prepare_legacy_policy_migration(
    mut policy: EscalationPolicy,
) -> Result<(EscalationPolicy, bool), ErrorData> {
    let mut changed = false;
    for channel in &mut policy.webhooks {
        let missing_channel_id = channel.channel_id.is_empty();
        if missing_channel_id {
            channel.channel_id = new_legacy_channel_id();
            changed = true;
        }
        let missing_generation = !policy
            .receiver_generations
            .contains_key(&channel.channel_id);
        if missing_generation {
            policy
                .receiver_generations
                .insert(channel.channel_id.clone(), new_receiver_generation());
            changed = true;
        }
        if (missing_channel_id || missing_generation)
            && channel.idempotency_contract != WebhookIdempotencyContract::UnsupportedLegacy
        {
            channel.idempotency_contract = WebhookIdempotencyContract::UnsupportedLegacy;
            changed = true;
        }
    }
    validate_policy(&policy, true, error_codes::STORAGE_CORRUPTED)?;
    Ok((policy, changed))
}

fn load_policy_revisioned(db: &Db) -> Result<RevisionedEscalationPolicy, ErrorData> {
    let key = CONFIG_KEY.as_bytes();
    for migration_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
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
        let (policy, migration_required) = prepare_legacy_policy_migration(policy)?;
        if !migration_required {
            return Ok(RevisionedEscalationPolicy {
                policy,
                revision_sha256: Some(revisioned.revision_sha256),
            });
        }

        let migrated_value = encode_json(&policy).map_err(|error| {
            mcp_error(
                error.code(),
                format!("legacy escalation policy migration encode failed: {error}"),
            )
        })?;
        let outcome = db
            .mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                [RevisionGuard::new(
                    key.to_vec(),
                    Some(revisioned.revision_sha256),
                )],
                std::iter::empty::<Vec<u8>>(),
                [(key.to_vec(), migrated_value.clone())],
            )
            .map_err(storage_error)?;
        if !outcome.applied {
            tracing::info!(
                code = "ESCALATION_POLICY_MIGRATION_REVISION_RETRY",
                migration_attempt,
                observed_seq = outcome.committed_seq,
                "legacy policy migration raced another writer; rereading without logging secret-bearing revisions"
            );
            continue;
        }
        if outcome.committed_revisions_sha256.len() != 1 {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "legacy escalation policy migration returned invalid revision shape: committed_seq={} revision_count={}",
                    outcome.committed_seq,
                    outcome.committed_revisions_sha256.len()
                ),
            ));
        }
        let committed_revision = outcome.committed_revisions_sha256[0].ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "legacy escalation policy migration committed without a row revision: committed_seq={}",
                    outcome.committed_seq
                ),
            )
        })?;
        let readback = db
            .get_cf_revisioned(cf::CF_KV, key)
            .map_err(storage_error)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    "legacy escalation policy disappeared immediately after migration",
                )
            })?;
        let readback_value = live_revisioned_value(&readback, "migrated escalation policy")?;
        if readback.revision_sha256 == committed_revision {
            if readback_value != migrated_value.as_slice() {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    "legacy escalation policy migration revision matched but bytes differed",
                ));
            }
            tracing::info!(
                code = "ESCALATION_POLICY_LEGACY_MIGRATED",
                committed_seq = outcome.committed_seq,
                disabled_receivers = policy.webhooks.len(),
                "readback=CF_KV legacy receivers received opaque identities and were disabled pending explicit receipt-v1 reconfiguration"
            );
            return Ok(RevisionedEscalationPolicy {
                policy,
                revision_sha256: Some(committed_revision),
            });
        }
        tracing::info!(
            code = "ESCALATION_POLICY_MIGRATION_SUPERSEDED",
            migration_attempt,
            committed_seq = outcome.committed_seq,
            "policy was superseded after migration; rereading authoritative bytes without logging secret-bearing revisions"
        );
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "legacy escalation policy migration could not acquire a stable revision after {ACK_REVISION_MAX_ATTEMPTS} attempts"
        ),
    ))
}

fn load_policy(db: &Db) -> Result<EscalationPolicy, ErrorData> {
    load_policy_revisioned(db).map(|revisioned| revisioned.policy)
}

fn store_policy(
    db: &Db,
    policy: &EscalationPolicy,
    expected_revision_sha256: Option<[u8; 32]>,
) -> Result<EscalationPolicy, ErrorData> {
    validate_policy(policy, false, error_codes::TOOL_PARAMS_INVALID)?;
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
        outcome.conflict.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                "escalation policy CAS reported applied=false without conflict detail",
            )
        })?;
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "escalation policy changed concurrently and was not overwritten: observed_seq={}; reread escalation_config_get and retry the intended update (secret-bearing policy revisions are intentionally not logged)",
                outcome.committed_seq
            ),
        ));
    }
    let committed_revision = outcome
        .committed_revisions_sha256
        .first()
        .copied()
        .flatten()
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "escalation policy CAS committed without exactly one row revision: committed_seq={} revision_count={}",
                    outcome.committed_seq,
                    outcome.committed_revisions_sha256.len()
                ),
            )
        })?;
    if outcome.committed_revisions_sha256.len() != 1 {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "escalation policy CAS returned an invalid revision shape: committed_seq={} revision_count={}",
                outcome.committed_seq,
                outcome.committed_revisions_sha256.len()
            ),
        ));
    }
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
    validate_policy(&authoritative_policy, false, error_codes::STORAGE_CORRUPTED)?;
    if readback.revision_sha256 != committed_revision {
        tracing::info!(
            code = "ESCALATION_POLICY_READBACK_SUPERSEDED",
            committed_seq = outcome.committed_seq,
            "separate policy readback decoded a newer valid policy revision; returning authoritative state without logging secret-bearing revisions"
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

fn verify_linked_approval_terminal(
    db: &Db,
    item: &EscalationItem,
    operation: &str,
) -> Result<(), ErrorData> {
    let approval_key = approval_item_key(&item.approval_id);
    let revisioned = db
        .get_cf_revisioned(cf::CF_KV, &approval_key)
        .map_err(storage_error)?;
    if item.approval_suppressed_reason.is_some() {
        if revisioned.is_some() {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "{operation} found a physical approval row for approval-suppressed escalation {}: approval_id={} key={}",
                    item.escalation_id,
                    item.approval_id,
                    hex_bytes(&approval_key)
                ),
            ));
        }
        return Ok(());
    }
    let revisioned = revisioned.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "{operation} lost the linked approval row for escalation {}: approval_id={} key={}",
                item.escalation_id,
                item.approval_id,
                hex_bytes(&approval_key)
            ),
        )
    })?;
    let approval_value = live_revisioned_value(
        &revisioned,
        &format!("{operation} linked approval {}", item.approval_id),
    )?;
    let approval = validate_linked_approval_item_identity(&approval_key, approval_value)?;
    validate_linked_approval_for_escalation(item, &approval)?;
    if !approval_status_is_terminal(approval.status) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "{operation} left linked approval non-terminal: escalation_id={} item_status={} approval_id={} approval_status={:?} key={} approval_revision={}",
                item.escalation_id,
                item.status.as_str(),
                item.approval_id,
                approval.status,
                hex_bytes(&approval_key),
                hex_bytes(&revisioned.revision_sha256)
            ),
        ));
    }
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

fn read_transition_projection_application(
    db: &Db,
    transition: &StateTransition,
    target_expected: bool,
) -> Result<TransitionProjectionApplication, ErrorData> {
    let items = scan_items(db)?;
    let anchor_items = items
        .iter()
        .filter(|item| item.anchor == transition.anchor)
        .collect::<Vec<_>>();
    let open_items = anchor_items
        .iter()
        .copied()
        .filter(|item| item.status.is_open())
        .collect::<Vec<_>>();
    let target_state = transition.state_to.as_str();
    if open_items
        .iter()
        .any(|item| item.attention_state != target_state)
        || (target_expected && open_items.len() != 1)
        || (!target_expected && !open_items.is_empty())
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition projection item invariant failed: anchor={:?} target_state={target_state:?} target_expected={target_expected} open_items={:?}",
                transition.anchor,
                open_items
                    .iter()
                    .map(|item| format!(
                        "{}:{}:{}",
                        item.escalation_id,
                        item.attention_state,
                        item.status.as_str()
                    ))
                    .collect::<Vec<_>>()
            ),
        ));
    }

    for item in &open_items {
        let index = read_open_index(db, &item.anchor, &item.attention_state)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "open projected escalation has no physical index: escalation_id={} anchor={:?} state={:?}",
                    item.escalation_id, item.anchor, item.attention_state
                ),
            )
        })?;
        if !index.record.is_open || index.record.escalation_id != item.escalation_id {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "open projected escalation/index mismatch: escalation_id={} index_escalation_id={} index_open={}",
                    item.escalation_id, index.record.escalation_id, index.record.is_open
                ),
            ));
        }
        let approval_key = approval_item_key(&item.approval_id);
        let approval = db
            .get_cf_revisioned(cf::CF_KV, &approval_key)
            .map_err(storage_error)?;
        if item.approval_suppressed_reason.is_some() {
            if approval.is_some() {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "approval-suppressed projected escalation unexpectedly has a physical approval row: escalation_id={} approval_id={}",
                        item.escalation_id, item.approval_id
                    ),
                ));
            }
        } else {
            let approval = approval.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "open projected escalation has no physical approval row: escalation_id={} approval_id={}",
                        item.escalation_id, item.approval_id
                    ),
                )
            })?;
            let approval = validate_linked_approval_item_identity(
                &approval_key,
                live_revisioned_value(&approval, "projected escalation approval")?,
            )?;
            validate_linked_approval_for_escalation(item, &approval)?;
            let approval_state_matches = match item.status {
                EscalationStatus::Pending => approval.status == ApprovalStatus::Pending,
                EscalationStatus::Acked => approval_status_is_terminal(approval.status),
                EscalationStatus::Resolved | EscalationStatus::Expired => false,
            };
            if !approval_state_matches {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "open projected escalation/approval status mismatch: escalation_id={} item_status={} approval_status={:?}",
                        item.escalation_id,
                        item.status.as_str(),
                        approval.status
                    ),
                ));
            }
        }
    }

    for item in anchor_items.iter().copied().filter(|item| {
        !item.status.is_open()
            && item.closed_reason.as_deref() == Some(&format!("state_change:{target_state}"))
    }) {
        if let Some(index) = read_open_index(db, &item.anchor, &item.attention_state)?
            && index.record.is_open
            && index.record.escalation_id == item.escalation_id
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "resolved obsolete escalation still has an open physical index: escalation_id={} state={:?}",
                    item.escalation_id, item.attention_state
                ),
            ));
        }
        if item.approval_suppressed_reason.is_none() {
            let approval_key = approval_item_key(&item.approval_id);
            let approval = db
                .get_cf_revisioned(cf::CF_KV, &approval_key)
                .map_err(storage_error)?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "resolved obsolete escalation lost its linked approval: escalation_id={} approval_id={}",
                            item.escalation_id, item.approval_id
                        ),
                    )
                })?;
            let approval = validate_linked_approval_item_identity(
                &approval_key,
                live_revisioned_value(&approval, "resolved escalation approval")?,
            )?;
            validate_linked_approval_for_escalation(item, &approval)?;
            if !approval_status_is_terminal(approval.status) {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "resolved obsolete escalation approval is not terminal: escalation_id={} approval_status={:?}",
                        item.escalation_id, approval.status
                    ),
                ));
            }
        }
    }
    if target_expected {
        let item = open_items.first().ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "transition projection application lost its required open item after invariant audit: anchor={:?} target_state={target_state:?}",
                    transition.anchor
                ),
            )
        })?;
        Ok(TransitionProjectionApplication::Escalation {
            escalation_id: item.escalation_id.clone(),
            approval_id: item.approval_id.clone(),
        })
    } else {
        let reason = projection_no_escalation_reason(transition).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "transition projection application found no item without a deterministic no-escalation reason: anchor={:?} state_to={target_state:?}",
                    transition.anchor
                ),
            )
        })?;
        Ok(TransitionProjectionApplication::NoEscalation { reason })
    }
}

fn transition_from_projection_input(
    input: &TransitionProjectionInput,
) -> Result<StateTransition, ErrorData> {
    let state_from = AgentLifecycleState::parse(&input.state_from).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "projection cursor has invalid state_from {:?} for anchor {:?}",
                input.state_from, input.anchor
            ),
        )
    })?;
    let state_to = AgentLifecycleState::parse(&input.state_to).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "projection cursor has invalid state_to {:?} for anchor {:?}",
                input.state_to, input.anchor
            ),
        )
    })?;
    Ok(StateTransition {
        anchor: input.anchor.clone(),
        spawn_id: input.spawn_id.clone(),
        session_id: input.session_id.clone(),
        state_from,
        state_to,
        reason_code: input.reason_code.clone(),
        waiting_for: input.waiting_for.clone(),
        runaway: input.runaway,
        evidence: input.evidence.clone(),
    })
}

fn verify_pending_projection_journal_source(
    db: &Db,
    cursor: &TransitionProjectionWatermark,
) -> Result<(), ErrorData> {
    let journal_key = synapse_storage::agent_events::agent_event_key(
        cursor.observed.generation.journal_ts_ns,
        cursor.observed.generation.journal_seq,
    );
    let witness = decode_projection_hex("journal_value_hex", &cursor.observed.journal_value_hex)?;
    match db
        .get_cf_revisioned(cf::CF_AGENT_EVENTS, &journal_key)
        .map_err(storage_error)?
    {
        Some(physical) => match physical.value.as_deref() {
            Some(value) if value == witness.as_slice() => {
                tracing::debug!(
                    code = "ESCALATION_PROJECTION_JOURNAL_SOURCE_VERIFIED",
                    anchor = %cursor.anchor,
                    journal_ts_ns = cursor.observed.generation.journal_ts_ns,
                    journal_seq = cursor.observed.generation.journal_seq,
                    journal_revision = %hex_bytes(&physical.revision_sha256),
                    journal_value_len = value.len(),
                    "Pending projection cursor independently matched its live physical journal source"
                );
            }
            Some(value) => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Pending transition projection cursor differs from its live physical journal source: anchor={:?} generation={:?} witness_len={} physical_len={}",
                        cursor.anchor,
                        cursor.observed.generation,
                        witness.len(),
                        value.len()
                    ),
                ));
            }
            None => {
                tracing::warn!(
                    code = "ESCALATION_PROJECTION_JOURNAL_SOURCE_RETIRED",
                    anchor = %cursor.anchor,
                    journal_ts_ns = cursor.observed.generation.journal_ts_ns,
                    journal_seq = cursor.observed.generation.journal_seq,
                    journal_revision = %hex_bytes(&physical.revision_sha256),
                    witness_value_len = witness.len(),
                    "Pending projection journal payload passed its retention horizon; the atomically committed cursor witness remains authoritative"
                );
            }
        },
        None => {
            tracing::warn!(
                code = "ESCALATION_PROJECTION_JOURNAL_SOURCE_RETIRED",
                anchor = %cursor.anchor,
                journal_ts_ns = cursor.observed.generation.journal_ts_ns,
                journal_seq = cursor.observed.generation.journal_seq,
                witness_value_len = witness.len(),
                "Pending projection journal row was physically retired; the atomically committed cursor witness remains authoritative"
            );
        }
    }
    Ok(())
}

fn projection_watermark_scan_bounds() -> (Vec<u8>, Vec<u8>) {
    let mut start = PROJECTION_WATERMARK_PREFIX.as_bytes().to_vec();
    start.resize(PROJECTION_WATERMARK_KEY_LEN, 0);
    let mut end = PROJECTION_WATERMARK_PREFIX.as_bytes().to_vec();
    for byte in end.iter_mut().rev() {
        if *byte != u8::MAX {
            *byte += 1;
            end.resize(PROJECTION_WATERMARK_KEY_LEN, 0);
            return (start, end);
        }
        *byte = 0;
    }
    unreachable!("ASCII projection watermark prefix always has a lexicographic successor")
}

fn validate_projection_watermark_page_key(key: &[u8]) -> Result<(), ErrorData> {
    let suffix = key.strip_prefix(PROJECTION_WATERMARK_PREFIX.as_bytes());
    if key.len() != PROJECTION_WATERMARK_KEY_LEN
        || suffix.is_none_or(|suffix| {
            suffix.len() != PROJECTION_ANCHOR_DIGEST_HEX_LEN
                || !suffix
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        })
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "fixed-width projection watermark audit returned a malformed or wrong-width row: expected_len={PROJECTION_WATERMARK_KEY_LEN} prefix={PROJECTION_WATERMARK_PREFIX:?} actual_len={} key={}",
                key.len(),
                hex_bytes(key)
            ),
        ));
    }
    Ok(())
}

fn read_pending_projection_index_migration(
    db: &Db,
) -> Result<Option<PendingProjectionIndexMigrationReadback>, ErrorData> {
    let key = PENDING_PROJECTION_INDEX_MIGRATION_KEY.as_bytes();
    db.get_cf_revisioned(cf::CF_KV, key)
        .map_err(storage_error)?
        .map(|revisioned| {
            let value = live_revisioned_value(
                &revisioned,
                "Pending projection index migration sentinel",
            )?;
            let record = decode_json::<PendingProjectionIndexMigrationRecord>(value).map_err(
                |error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "Pending projection index migration sentinel decode failed: key={} error={error}",
                            hex_bytes(key)
                        ),
                    )
                },
            )?;
            if record.schema_version != SCHEMA_VERSION
                || record.index_version != PENDING_PROJECTION_INDEX_VERSION
                || record.migration_version != 1
                || record.completed_at_unix_ms == 0
                || record
                    .pending_rows_indexed
                    .saturating_add(record.applied_rows_verified)
                    != record.cursor_rows_audited
                || !is_lower_hex_exact(&record.audit_sha256, 64)
            {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Pending projection index migration sentinel invariant failed: key={} schema_version={} index_version={} migration_version={} completed_at_unix_ms={} cursor_rows_audited={} pending_rows_indexed={} applied_rows_verified={} audit_sha256={:?}",
                        hex_bytes(key),
                        record.schema_version,
                        record.index_version,
                        record.migration_version,
                        record.completed_at_unix_ms,
                        record.cursor_rows_audited,
                        record.pending_rows_indexed,
                        record.applied_rows_verified,
                        record.audit_sha256
                    ),
                ));
            }
            Ok(PendingProjectionIndexMigrationReadback {
                record,
                value: value.to_vec(),
                revision_sha256: revisioned.revision_sha256,
            })
        })
        .transpose()
}

fn append_projection_migration_audit_witness(
    digest: &mut Sha256,
    cursor_key: &[u8],
    cursor: &RevisionedTransitionProjectionWatermark,
    index_revision_sha256: Option<[u8; 32]>,
) {
    digest.update(
        u64::try_from(cursor_key.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(cursor_key);
    digest.update(cursor.revision_sha256);
    digest.update(Sha256::digest(&cursor.value));
    digest.update([match cursor.record.phase {
        TransitionProjectionPhase::Pending => 1,
        TransitionProjectionPhase::Applied => 2,
    }]);
    match index_revision_sha256 {
        Some(revision) => {
            digest.update([1]);
            digest.update(revision);
        }
        None => digest.update([0]),
    }
}

fn migrate_one_pending_projection_index(
    db: &Db,
    cursor_key: &[u8],
    cursor: &RevisionedTransitionProjectionWatermark,
) -> Result<RevisionedPendingTransitionProjectionIndex, ErrorData> {
    let (index_key, index_record) = pending_projection_index_record(
        cursor_key,
        &cursor.value,
        &cursor.record,
        Some(cursor.revision_sha256),
        None,
    )?;
    let index_value = encode_json(&index_record).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "Pending projection migration index encode failed: anchor={:?} generation={:?} error={error}",
                cursor.record.anchor, cursor.record.observed.generation
            ),
        )
    })?;
    let outcome = db
        .mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [
                RevisionGuard::new(cursor_key.to_vec(), Some(cursor.revision_sha256)),
                RevisionGuard::new(index_key.clone(), None),
            ],
            std::iter::empty::<Vec<u8>>(),
            [(index_key.clone(), index_value.clone())],
        )
        .map_err(storage_error)?;
    if !outcome.applied {
        let conflict = outcome.conflict.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                "Pending projection migration reported applied=false without conflict evidence",
            )
        })?;
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection migration lost an exclusive revision guard: anchor={:?} generation={:?} conflict_key={} expected_revision={} actual_revision={} observed_seq={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                hex_bytes(&conflict.key),
                conflict
                    .expected_revision_sha256
                    .as_ref()
                    .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision)),
                conflict
                    .actual_revision_sha256
                    .as_ref()
                    .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision)),
                outcome.committed_seq
            ),
        ));
    }
    if outcome.committed_revisions_sha256.len() != 2
        || outcome.committed_revisions_sha256[0] != Some(cursor.revision_sha256)
    {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "Pending projection migration returned invalid revision shape: anchor={:?} generation={:?} committed_seq={} revisions={} cursor_revision_preserved={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                outcome.committed_seq,
                outcome.committed_revisions_sha256.len(),
                outcome
                    .committed_revisions_sha256
                    .first()
                    .copied()
                    .flatten()
                    == Some(cursor.revision_sha256)
            ),
        ));
    }
    let committed_index_revision = outcome.committed_revisions_sha256[1].ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "Pending projection migration committed without an index revision: anchor={:?} generation={:?} committed_seq={}",
                cursor.record.anchor, cursor.record.observed.generation, outcome.committed_seq
            ),
        )
    })?;
    let cursor_readback = read_projection_watermark(db, &cursor.record.anchor)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "projection cursor disappeared after Pending-index migration: anchor={:?} generation={:?} cursor_key={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                hex_bytes(cursor_key)
            ),
        )
    })?;
    if cursor_readback.revision_sha256 != cursor.revision_sha256
        || cursor_readback.value != cursor.value
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "projection cursor drifted across Pending-index migration readback: anchor={:?} generation={:?} revision_matches={} bytes_match={} cursor_key={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                cursor_readback.revision_sha256 == cursor.revision_sha256,
                cursor_readback.value == cursor.value,
                hex_bytes(cursor_key)
            ),
        ));
    }
    let index_readback = read_pending_projection_index(db, &cursor.record.anchor)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection index absent immediately after migration: anchor={:?} generation={:?} index_key={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                hex_bytes(&index_key)
            ),
        )
    })?;
    if index_readback.revision_sha256 != committed_index_revision
        || index_readback.value != index_value
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection index migration readback mismatch: anchor={:?} generation={:?} revision_matches={} bytes_match={} index_key={}",
                cursor.record.anchor,
                cursor.record.observed.generation,
                index_readback.revision_sha256 == committed_index_revision,
                index_readback.value == index_value,
                hex_bytes(&index_key)
            ),
        ));
    }
    validate_pending_projection_index_binding(
        &index_key,
        &index_readback.record,
        cursor_key,
        &cursor.value,
        &cursor.record,
    )?;
    tracing::warn!(
        code = "ESCALATION_PENDING_PROJECTION_INDEX_MIGRATED",
        anchor = %cursor.record.anchor,
        journal_ts_ns = cursor.record.observed.generation.journal_ts_ns,
        journal_seq = cursor.record.observed.generation.journal_seq,
        cursor_key = %String::from_utf8_lossy(cursor_key),
        cursor_revision = %hex_bytes(&cursor.revision_sha256),
        index_key = %String::from_utf8_lossy(&index_key),
        index_revision = %hex_bytes(&index_readback.revision_sha256),
        committed_seq = outcome.committed_seq,
        "legacy Pending cursor was revision-guarded into the durable delta-first work index"
    );
    Ok(index_readback)
}

fn ensure_pending_projection_index_migration_locked(db: &Db) -> Result<(), ErrorData> {
    if let Some(readback) = read_pending_projection_index_migration(db)? {
        tracing::debug!(
            code = "ESCALATION_PENDING_PROJECTION_INDEX_MIGRATION_VERIFIED",
            migration_revision = %hex_bytes(&readback.revision_sha256),
            completed_at_unix_ms = readback.record.completed_at_unix_ms,
            cursor_rows_audited = readback.record.cursor_rows_audited,
            pending_rows_indexed = readback.record.pending_rows_indexed,
            applied_rows_verified = readback.record.applied_rows_verified,
            audit_sha256 = %readback.record.audit_sha256,
            "durable Pending-projection migration sentinel passed exact point readback"
        );
        return Ok(());
    }

    let (start, end) = projection_watermark_scan_bounds();
    let mut lease = db
        .pin_cf_fixed_width_range_scan(
            cf::CF_KV,
            &start,
            &end,
            synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS,
        )
        .map_err(storage_error)?;
    let lease_id = lease.lease_id;
    let pinned_snapshot_seq = lease.snapshot_seq;
    let mut cursor_rows_audited = 0_u64;
    let mut pending_rows_indexed = 0_u64;
    let mut applied_rows_verified = 0_u64;
    let mut pages = 0_u64;
    let mut audit = Sha256::new();
    audit.update(b"synapse.escalation.pending-projection-index-migration.v1\0");
    let scan_result = (|| -> Result<(), ErrorData> {
        loop {
            let page = db
                .scan_cf_fixed_width_range_page_coherent(&mut lease, PENDING_PROJECTION_PAGE_ROWS)
                .map_err(storage_error)?;
            let snapshot_seq = page.snapshot_seq.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    "projection migration coherent page omitted its pinned Calyx snapshot sequence",
                )
            })?;
            if snapshot_seq != pinned_snapshot_seq {
                return Err(mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "projection migration coherent page escaped its pinned generation: lease_id={lease_id} expected_snapshot_seq={pinned_snapshot_seq} actual_snapshot_seq={snapshot_seq}"
                    ),
                ));
            }
            pages = pages.saturating_add(1);
            let page_rows = page.rows.len();
            for (cursor_key, scanned_value) in page.rows {
                validate_projection_watermark_page_key(&cursor_key)?;
                let physical = db
                    .get_cf_revisioned(cf::CF_KV, &cursor_key)
                    .map_err(storage_error)?
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "projection cursor disappeared during coherent migration audit: lease_id={lease_id} snapshot_seq={pinned_snapshot_seq} cursor_key={}",
                                hex_bytes(&cursor_key)
                            ),
                        )
                    })?;
                let physical_value =
                    live_revisioned_value(&physical, "projection migration cursor point read")?;
                if physical_value != scanned_value {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "projection cursor drifted while the transition lock and coherent migration snapshot were held: lease_id={lease_id} snapshot_seq={pinned_snapshot_seq} cursor_key={} scanned_sha256={} physical_sha256={} cursor_revision={}",
                            hex_bytes(&cursor_key),
                            hex_bytes(&Sha256::digest(&scanned_value)),
                            hex_bytes(&Sha256::digest(physical_value)),
                            hex_bytes(&physical.revision_sha256)
                        ),
                    ));
                }
                let record = decode_json::<TransitionProjectionWatermark>(physical_value).map_err(
                    |error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "projection cursor decode failed during coherent migration audit: lease_id={lease_id} snapshot_seq={pinned_snapshot_seq} cursor_key={} error={error}",
                                hex_bytes(&cursor_key)
                            ),
                        )
                    },
                )?;
                validate_projection_watermark(&cursor_key, &record)?;
                let cursor = RevisionedTransitionProjectionWatermark {
                    record,
                    value: physical_value.to_vec(),
                    revision_sha256: physical.revision_sha256,
                };
                let index = match cursor.record.phase {
                    TransitionProjectionPhase::Pending => {
                        pending_rows_indexed = pending_rows_indexed.saturating_add(1);
                        match read_pending_projection_index(db, &cursor.record.anchor)? {
                            Some(index) => {
                                validate_pending_projection_index_binding(
                                    &pending_projection_index_key(&cursor.record.anchor),
                                    &index.record,
                                    &cursor_key,
                                    &cursor.value,
                                    &cursor.record,
                                )?;
                                index
                            }
                            None => migrate_one_pending_projection_index(db, &cursor_key, &cursor)?,
                        }
                    }
                    TransitionProjectionPhase::Applied => {
                        applied_rows_verified = applied_rows_verified.saturating_add(1);
                        verify_applied_projection_evidence(db, &cursor.record)?;
                        if let Some(index) =
                            read_pending_projection_index(db, &cursor.record.anchor)?
                        {
                            return Err(mcp_error(
                                error_codes::STORAGE_CORRUPTED,
                                format!(
                                    "Applied cursor retained Pending work during migration audit: anchor={:?} generation={:?} cursor_key={} index_key={} index_revision={}",
                                    cursor.record.anchor,
                                    cursor.record.observed.generation,
                                    hex_bytes(&cursor_key),
                                    hex_bytes(&pending_projection_index_key(&cursor.record.anchor)),
                                    hex_bytes(&index.revision_sha256)
                                ),
                            ));
                        }
                        append_projection_migration_audit_witness(
                            &mut audit,
                            &cursor_key,
                            &cursor,
                            None,
                        );
                        cursor_rows_audited = cursor_rows_audited.saturating_add(1);
                        continue;
                    }
                };
                append_projection_migration_audit_witness(
                    &mut audit,
                    &cursor_key,
                    &cursor,
                    Some(index.revision_sha256),
                );
                cursor_rows_audited = cursor_rows_audited.saturating_add(1);
            }
            tracing::debug!(
                code = "ESCALATION_PENDING_PROJECTION_MIGRATION_PAGE_AUDITED",
                page_number = pages,
                page_rows,
                candidate_rows_examined = page.candidate_rows_examined,
                expired_rows_skipped = page.expired_rows_skipped,
                snapshot_seq,
                lease_id,
                requested_candidate_rows = PENDING_PROJECTION_PAGE_ROWS,
                "processed one bounded projection-cursor page from the pinned migration generation"
            );
            if !page.more {
                break;
            }
        }
        Ok(())
    })();
    finish_coherent_scan(
        db,
        &mut lease,
        "ESCALATION_PENDING_PROJECTION_INDEX_MIGRATION",
        scan_result,
    )?;

    let record = PendingProjectionIndexMigrationRecord {
        schema_version: SCHEMA_VERSION,
        index_version: PENDING_PROJECTION_INDEX_VERSION,
        migration_version: 1,
        completed_at_unix_ms: unix_time_ms_now(),
        cursor_rows_audited,
        pending_rows_indexed,
        applied_rows_verified,
        audit_sha256: hex_bytes(&audit.finalize()),
    };
    let value = encode_json(&record).map_err(|error| {
        mcp_error(
            error.code(),
            format!("Pending projection migration sentinel encode failed: {error}"),
        )
    })?;
    let key = PENDING_PROJECTION_INDEX_MIGRATION_KEY.as_bytes();
    let outcome = db
        .put_batch_if_revision_pressure_bypass(
            cf::CF_KV,
            key,
            None,
            [(key.to_vec(), value.clone())],
        )
        .map_err(storage_error)?;
    if !outcome.applied {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection migration sentinel unexpectedly raced at startup: key={} actual_revision={} observed_seq={}",
                hex_bytes(key),
                outcome
                    .previous_revision_sha256
                    .as_ref()
                    .map_or_else(|| "absent".to_owned(), |revision| hex_bytes(revision)),
                outcome.committed_seq
            ),
        ));
    }
    let committed_revision = outcome.committed_revision_sha256.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            "Pending projection migration sentinel commit returned no revision",
        )
    })?;
    let readback = read_pending_projection_index_migration(db)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            "Pending projection migration sentinel absent immediately after commit",
        )
    })?;
    if readback.record != record
        || readback.value != value
        || readback.revision_sha256 != committed_revision
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Pending projection migration sentinel readback mismatch: key={} record_matches={} bytes_match={} revision_matches={} committed_revision={} actual_revision={}",
                hex_bytes(key),
                readback.record == record,
                readback.value == value,
                readback.revision_sha256 == committed_revision,
                hex_bytes(&committed_revision),
                hex_bytes(&readback.revision_sha256)
            ),
        ));
    }
    tracing::info!(
        code = "ESCALATION_PENDING_PROJECTION_INDEX_MIGRATION_COMPLETED",
        migration_key = PENDING_PROJECTION_INDEX_MIGRATION_KEY,
        migration_revision = %hex_bytes(&readback.revision_sha256),
        completed_at_unix_ms = record.completed_at_unix_ms,
        cursor_rows_audited,
        pending_rows_indexed,
        applied_rows_verified,
        pages,
        lease_id,
        snapshot_seq = pinned_snapshot_seq,
        audit_sha256 = %record.audit_sha256,
        "durable one-time cursor audit completed from one pinned Calyx generation before delta-first Pending reconciliation"
    );
    Ok(())
}

fn ensure_pending_projection_index_migration(db: &Db) -> Result<(), ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| {
        ensure_pending_projection_index_migration_locked(db)
    })
    .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("Pending projection index migration lock failed: {detail}"),
        )
    })?
}

fn pending_projection_index_scan_bounds() -> (Vec<u8>, Vec<u8>) {
    let mut start = PENDING_PROJECTION_INDEX_PREFIX.as_bytes().to_vec();
    start.resize(PENDING_PROJECTION_INDEX_KEY_LEN, 0);
    let mut end = PENDING_PROJECTION_INDEX_PREFIX.as_bytes().to_vec();
    for byte in end.iter_mut().rev() {
        if *byte != u8::MAX {
            *byte += 1;
            end.resize(PENDING_PROJECTION_INDEX_KEY_LEN, 0);
            return (start, end);
        }
        *byte = 0;
    }
    unreachable!("ASCII Pending projection index prefix always has a lexicographic successor")
}

fn validate_pending_projection_page_key(key: &[u8]) -> Result<(), ErrorData> {
    let suffix = key.strip_prefix(PENDING_PROJECTION_INDEX_PREFIX.as_bytes());
    if key.len() != PENDING_PROJECTION_INDEX_KEY_LEN
        || suffix.is_none_or(|suffix| {
            suffix.len() != PROJECTION_ANCHOR_DIGEST_HEX_LEN
                || !suffix
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        })
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "fixed-width Pending projection page returned a malformed or wrong-width row: expected_len={PENDING_PROJECTION_INDEX_KEY_LEN} prefix={PENDING_PROJECTION_INDEX_PREFIX:?} actual_len={} key={}",
                key.len(),
                hex_bytes(key)
            ),
        ));
    }
    Ok(())
}

fn next_pending_projection_page_cursor(
    page: &synapse_storage::FixedWidthScanPage,
    previous: Option<&[u8]>,
    start: &[u8],
    end: &[u8],
) -> Result<Option<Vec<u8>>, ErrorData> {
    if !page.more {
        return Ok(None);
    }
    let cursor = page.resume_after.clone().ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            "Pending projection fixed-width page reported more candidates without a resume cursor",
        )
    })?;
    validate_pending_projection_page_key(&cursor)?;
    if cursor.as_slice() < start
        || cursor.as_slice() >= end
        || previous.is_some_and(|previous| cursor.as_slice() <= previous)
    {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "Pending projection fixed-width page returned an out-of-range or non-progressing cursor: start={} end={} previous={} current={}",
                hex_bytes(start),
                hex_bytes(end),
                previous.map_or_else(|| "none".to_owned(), hex_bytes),
                hex_bytes(&cursor)
            ),
        ));
    }
    Ok(Some(cursor))
}

#[derive(Clone, Debug)]
struct PendingProjectionPageReconcile {
    reconciled: usize,
    candidate_rows_examined: usize,
    expired_rows_skipped: usize,
    snapshot_seq: u64,
    next_after: Option<Vec<u8>>,
}

fn reconcile_pending_projection_page_locked(
    db: &Db,
    after_key: Option<&[u8]>,
) -> Result<PendingProjectionPageReconcile, ErrorData> {
    // This is deliberately an eventual/restarting work-queue drain, not a
    // one-generation audit. Each page is atomic and every candidate is
    // revision/binding checked while the transition lock is held. Inserts
    // behind `after_key` are picked up when the recurring worker restarts from
    // the prefix; no completion sentinel is written from this pass.
    let (start, end) = pending_projection_index_scan_bounds();
    let page = db
        .scan_cf_fixed_width_range_page(
            cf::CF_KV,
            &start,
            &end,
            after_key,
            PENDING_PROJECTION_PAGE_ROWS,
        )
        .map_err(storage_error)?;
    let snapshot_seq = page.snapshot_seq.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            "Pending projection fixed-width page omitted its atomic Calyx snapshot sequence",
        )
    })?;
    let next_after =
        next_pending_projection_page_cursor(&page, after_key, start.as_slice(), end.as_slice())?;
    let candidate_rows_examined = page.candidate_rows_examined;
    let expired_rows_skipped = page.expired_rows_skipped;
    let mut reconciled = 0_usize;
    for (index_key, scanned_index_value) in page.rows {
        validate_pending_projection_page_key(&index_key)?;
        let scanned_index = decode_json::<PendingTransitionProjectionIndex>(&scanned_index_value)
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "Pending projection index decode failed during reconciliation: key={} error={error}",
                        hex_bytes(&index_key)
                    ),
                )
            })?;
        validate_pending_projection_index(&index_key, &scanned_index)?;
        let physical_index = read_pending_projection_index(db, &scanned_index.anchor)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Pending projection index disappeared between bounded page scan and exact point read: anchor={:?} generation={:?} key={}",
                        scanned_index.anchor,
                        scanned_index.generation,
                        hex_bytes(&index_key)
                    ),
                )
            })?;
        if physical_index.value != scanned_index_value {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending projection index drifted between bounded page scan and exact point read: anchor={:?} generation={:?} key={} scanned_sha256={} physical_sha256={} index_revision={}",
                    scanned_index.anchor,
                    scanned_index.generation,
                    hex_bytes(&index_key),
                    hex_bytes(&Sha256::digest(&scanned_index_value)),
                    hex_bytes(&Sha256::digest(&physical_index.value)),
                    hex_bytes(&physical_index.revision_sha256)
                ),
            ));
        }
        let cursor_key = projection_watermark_key(&scanned_index.anchor);
        let cursor = read_projection_watermark(db, &scanned_index.anchor)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending projection index has no exact cursor during reconciliation: anchor={:?} generation={:?} index_key={} cursor_key={}",
                    scanned_index.anchor,
                    scanned_index.generation,
                    hex_bytes(&index_key),
                    hex_bytes(&cursor_key)
                ),
            )
        })?;
        validate_pending_projection_index_binding(
            &index_key,
            &physical_index.record,
            &cursor_key,
            &cursor.value,
            &cursor.record,
        )?;
        verify_pending_projection_journal_source(db, &cursor.record)?;
        let transition = transition_from_projection_input(&cursor.record.observed)?;
        note_transition_locked(
            db,
            &transition,
            cursor.record.observed.generation,
            unix_time_ms_now(),
        )?;
        let after_cursor = read_projection_watermark(db, &transition.anchor)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "projection cursor disappeared after Pending-index reconciliation: anchor={:?} expected_generation={:?} cursor_key={}",
                    transition.anchor,
                    cursor.record.observed.generation,
                    hex_bytes(&cursor_key)
                ),
            )
        })?;
        if after_cursor.record.observed.generation != cursor.record.observed.generation
            || after_cursor.record.phase != TransitionProjectionPhase::Applied
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending-index reconciliation did not leave the exact cursor generation Applied: anchor={:?} expected={:?} actual={:?} phase={:?} cursor_revision={}",
                    transition.anchor,
                    cursor.record.observed.generation,
                    after_cursor.record.observed.generation,
                    after_cursor.record.phase,
                    hex_bytes(&after_cursor.revision_sha256)
                ),
            ));
        }
        if let Some(after_index) = read_pending_projection_index(db, &transition.anchor)? {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Pending-index reconciliation left its work row live after Applied completion: anchor={:?} generation={:?} index_key={} index_revision={}",
                    transition.anchor,
                    cursor.record.observed.generation,
                    hex_bytes(&index_key),
                    hex_bytes(&after_index.revision_sha256)
                ),
            ));
        }
        reconciled = reconciled.saturating_add(1);
    }
    Ok(PendingProjectionPageReconcile {
        reconciled,
        candidate_rows_examined,
        expired_rows_skipped,
        snapshot_seq,
        next_after,
    })
}

fn reconcile_pending_projection_page(
    db: &Db,
    after_key: Option<&[u8]>,
) -> Result<PendingProjectionPageReconcile, ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| {
        reconcile_pending_projection_page_locked(db, after_key)
    })
    .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("transition projection reconciliation lock failed: {detail}"),
        )
    })?
}

/// Startup reconciliation drains durable Pending work through bounded physical
/// pages. Applied cursors are deliberately not part of this recurring workset;
/// their evidence remains exact-point-verifiable at claim/audit boundaries.
/// This eventual pass never claims one-snapshot coverage and the worker repeats
/// it from the prefix so concurrent inserts behind a page cursor remain work.
pub(crate) fn reconcile_transition_projections(db: &Db) -> Result<usize, ErrorData> {
    ensure_pending_projection_index_migration(db)?;
    let mut after_key = None;
    let mut reconciled = 0_usize;
    let mut pages = 0_usize;
    let mut candidate_rows_examined = 0_usize;
    let mut expired_rows_skipped = 0_usize;
    loop {
        let page = reconcile_pending_projection_page(db, after_key.as_deref())?;
        pages = pages.saturating_add(1);
        reconciled = reconciled.saturating_add(page.reconciled);
        candidate_rows_examined =
            candidate_rows_examined.saturating_add(page.candidate_rows_examined);
        expired_rows_skipped = expired_rows_skipped.saturating_add(page.expired_rows_skipped);
        tracing::debug!(
            code = "ESCALATION_PENDING_PROJECTION_PAGE_RECONCILED",
            mode = "startup",
            page_number = pages,
            page_reconciled = page.reconciled,
            candidate_rows_examined = page.candidate_rows_examined,
            expired_rows_skipped = page.expired_rows_skipped,
            snapshot_seq = page.snapshot_seq,
            requested_candidate_rows = PENDING_PROJECTION_PAGE_ROWS,
            pending_index_key_len = PENDING_PROJECTION_INDEX_KEY_LEN,
            "processed and discarded one bounded physical Pending-projection index page"
        );
        let Some(next) = page.next_after else {
            break;
        };
        after_key = Some(next);
    }
    tracing::info!(
        code = "ESCALATION_PROJECTION_RECONCILED",
        mode = "startup",
        reconciled,
        pages,
        candidate_rows_examined,
        expired_rows_skipped,
        pending_index_prefix = PENDING_PROJECTION_INDEX_PREFIX,
        pending_index_key_len = PENDING_PROJECTION_INDEX_KEY_LEN,
        "startup Pending transition projection pass reached the end of its eventual/restarting workset"
    );
    Ok(reconciled)
}

async fn reconcile_transition_projections_cancellable(
    db: &Db,
    shutdown: &CancellationToken,
) -> Result<Option<usize>, ErrorData> {
    if read_pending_projection_index_migration(db)?.is_none() {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "escalation worker started without the durable Pending-projection migration sentinel: key={}; startup must finish its bounded cursor audit before side effects",
                hex_bytes(PENDING_PROJECTION_INDEX_MIGRATION_KEY.as_bytes())
            ),
        ));
    }
    let mut after_key = None;
    let mut reconciled = 0_usize;
    let mut pages = 0_usize;
    let mut candidate_rows_examined = 0_usize;
    let mut expired_rows_skipped = 0_usize;
    loop {
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        let page_started = Instant::now();
        let page = reconcile_pending_projection_page(db, after_key.as_deref())?;
        pages = pages.saturating_add(1);
        reconciled = reconciled.saturating_add(page.reconciled);
        candidate_rows_examined =
            candidate_rows_examined.saturating_add(page.candidate_rows_examined);
        expired_rows_skipped = expired_rows_skipped.saturating_add(page.expired_rows_skipped);
        let page_elapsed_ms = page_started.elapsed().as_millis();
        if page.reconciled > 0 || page_elapsed_ms >= WORKER_SLOW_SCAN_LOG_MS {
            tracing::info!(
                code = "ESCALATION_PENDING_PROJECTION_PAGE_RECONCILED",
                mode = "worker",
                page_number = pages,
                page_reconciled = page.reconciled,
                page_elapsed_ms,
                candidate_rows_examined = page.candidate_rows_examined,
                expired_rows_skipped = page.expired_rows_skipped,
                snapshot_seq = page.snapshot_seq,
                requested_candidate_rows = PENDING_PROJECTION_PAGE_ROWS,
                pending_index_key_len = PENDING_PROJECTION_INDEX_KEY_LEN,
                "processed and discarded one bounded physical Pending-projection index page"
            );
        }
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        let Some(next) = page.next_after else {
            break;
        };
        after_key = Some(next);
        tokio::task::yield_now().await;
    }
    if reconciled > 0 {
        tracing::info!(
            code = "ESCALATION_PROJECTION_RECONCILED",
            mode = "worker",
            reconciled,
            pages,
            candidate_rows_examined,
            expired_rows_skipped,
            pending_index_prefix = PENDING_PROJECTION_INDEX_PREFIX,
            "one eventual/restarting Pending transition projection pass reconciled work without scanning Applied cursors"
        );
    }
    Ok(Some(reconciled))
}

/// Hook called once per live state transition after the authoritative
/// `state_changed` rows committed. Opens or resolves durable escalations. A
/// storage failure here is logged loudly but never unwinds the caller — the
/// primary journal rows already committed and the attention state is
/// re-derivable from them.
pub(crate) fn note_transition(
    db: &Db,
    transition: &StateTransition,
    journal_ts_ns: u64,
    journal_seq: u32,
    now_unix_ms: u64,
) {
    let generation = TransitionGeneration {
        journal_ts_ns,
        journal_seq,
    };
    let projection = match super::agent_state::with_transition_pipeline_lock(|| {
        note_transition_locked(db, transition, generation, now_unix_ms)
    }) {
        Ok(projection) => projection,
        Err(detail) => Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "live transition projection lock failed: anchor={:?} generation={generation:?} detail={detail}",
                transition.anchor
            ),
        )),
    };
    if let Err(error) = projection {
        tracing::error!(
            code = "ESCALATION_TRANSITION_FAILED",
            anchor = %transition.anchor,
            state_to = transition.state_to.as_str(),
            journal_ts_ns,
            journal_seq,
            detail = %error.message,
            "escalation engine could not record a state transition; attention escalation may be missed for this edge"
        );
    }
}

/// Executes one projection while the global transition pipeline lock is held.
/// Both the live callback and restart/worker reconciliation enter through a
/// lock-owning wrapper, so a cursor cannot legally change between scan,
/// point-read, and its guarded projection mutations.
fn note_transition_locked(
    db: &Db,
    transition: &StateTransition,
    generation: TransitionGeneration,
    now_unix_ms: u64,
) -> Result<(), ErrorData> {
    if generation.journal_ts_ns == 0 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition for anchor {:?} has zero authoritative journal timestamp",
                transition.anchor
            ),
        ));
    }
    let authoritative = read_projection_watermark(db, &transition.anchor)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "committed transition has no durable projection cursor: anchor={:?} generation={generation:?}",
                transition.anchor
            ),
        )
    })?;
    if authoritative.record.observed.generation > generation {
        tracing::warn!(
            code = "ESCALATION_TRANSITION_STALE",
            anchor = %transition.anchor,
            incoming_state = transition.state_to.as_str(),
            incoming_journal_ts_ns = generation.journal_ts_ns,
            incoming_journal_seq = generation.journal_seq,
            authoritative_state = %authoritative.record.observed.state_to,
            authoritative_journal_ts_ns = authoritative.record.observed.generation.journal_ts_ns,
            authoritative_journal_seq = authoritative.record.observed.generation.journal_seq,
            "older transition callback was rejected before reading or mutating escalation items"
        );
        return Ok(());
    }
    if authoritative.record.observed.generation < generation
        || !transition_matches_projection_input(
            transition,
            generation,
            &authoritative.record.observed,
        )
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "transition callback does not match its durable complete cursor: anchor={:?} callback_generation={generation:?} cursor_generation={:?}",
                transition.anchor, authoritative.record.observed.generation
            ),
        ));
    }
    let new_state = transition.state_to.as_str();
    let policy_suppressed_reason = operator_interrupt_suppressed_reason(transition);
    let target_expected =
        severity_for(transition.state_to).is_some() && policy_suppressed_reason.is_none();
    if authoritative.record.phase == TransitionProjectionPhase::Applied {
        verify_applied_projection_evidence(db, &authoritative.record)?;
        return Ok(());
    }
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
                || (target_expected && current.item.attention_state == new_state)
            {
                resolved_or_superseded = true;
                break;
            }
            let item = &mut current.item;
            item.status = EscalationStatus::Resolved;
            item.updated_at_unix_ms = now_unix_ms.max(item.updated_at_unix_ms);
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
            match projection_rows_for_mutation(db, transition, generation, now_unix_ms)? {
                ProjectionMutationRows::Ready(rows) => approval_rows.extend(rows),
                ProjectionMutationRows::Stale(authoritative) => {
                    tracing::warn!(
                        code = "ESCALATION_TRANSITION_STALE",
                        anchor = %transition.anchor,
                        escalation_id = %item.escalation_id,
                        incoming_journal_ts_ns = generation.journal_ts_ns,
                        incoming_journal_seq = generation.journal_seq,
                        authoritative_journal_ts_ns = authoritative.observed.generation.journal_ts_ns,
                        authoritative_journal_seq = authoritative.observed.generation.journal_seq,
                        "newer transition cursor won before obsolete-item resolution; stale callback stopped"
                    );
                    return Ok(());
                }
                ProjectionMutationRows::Applied => {
                    verify_applied_projection_for_transition(db, transition, generation)?;
                    return Ok(());
                }
            }
            match write_item_and_audit_with_extra_rows_if_revision(
                db,
                item,
                "resolved",
                json!({
                    "reason": "state_change",
                    "new_state": new_state,
                    "journal_ts_ns": generation.journal_ts_ns,
                    "journal_seq": generation.journal_seq,
                }),
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

    // 2. Open a new escalation only when the final transition requires one.
    //    The cursor stays Pending throughout this step.
    let severity = severity_for(transition.state_to);
    if let Some(policy_suppressed_reason) = &policy_suppressed_reason {
        tracing::info!(
            code = "ESCALATION_SUPPRESSED",
            anchor = %transition.anchor,
            state_to = transition.state_to.as_str(),
            reason_code = %transition.reason_code,
            policy_suppressed_reason = %policy_suppressed_reason,
            "operator-facing escalation suppressed by policy before item creation"
        );
    } else if let Some(severity) = severity {
        let already_open = open_items_for_anchor(db, &transition.anchor)?
            .into_iter()
            .any(|item| item.attention_state == new_state);
        if !already_open {
            let policy = load_policy_revisioned(db)?;
            if open_escalation(db, transition, severity, &policy, generation, now_unix_ms)?
                .is_some()
            {
                superseded = true;
            }
        }
    }

    // 3. A separate physical read audit is the only authority allowed to
    //    complete the cursor. A crash anywhere above leaves Pending and is
    //    resumed at startup/worker reconciliation.
    let application = read_transition_projection_application(db, transition, target_expected)?;
    if !persist_projection_watermark_only(db, transition, generation, now_unix_ms, &application)? {
        return Ok(());
    }
    if superseded {
        wake_worker();
    }
    Ok(())
}

fn open_escalation(
    db: &Db,
    transition: &StateTransition,
    severity: Severity,
    revisioned_policy: &RevisionedEscalationPolicy,
    generation: TransitionGeneration,
    now_unix_ms: u64,
) -> Result<Option<EscalationItem>, ErrorData> {
    let policy = &revisioned_policy.policy;
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
        tier0_delivery: policy_suppressed_reason.as_ref().map_or(
            Tier0ToastDelivery::NotRequested,
            |reason| Tier0ToastDelivery::Suppressed {
                reason: reason.clone(),
                at_unix_ms: now_unix_ms,
            },
        ),
        tier0_payload_sha256: None,
        tier0_prepared_payload: None,
        tier0_toast_removed: None,
        tier0_quiet_digest: quiet_suppressed || policy_suppressed_reason.is_some(),
        tier0_suppressed_reason: policy_suppressed_reason.clone(),
        tier1_quiet_suppressed: quiet_suppressed,
        tier1_suppressed_reason: policy_suppressed_reason.clone(),
        approval_suppressed_reason: policy_suppressed_reason.clone(),
        tier1_eligible,
        webhook_channel_ids: policy
            .webhooks
            .iter()
            .map(|channel| channel.channel_id.clone())
            .collect(),
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
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let prior_index = read_open_index(db, &transition.anchor, transition.state_to.as_str())?;
        if let Some(index) = &prior_index
            && index.record.is_open
        {
            let winner = indexed_open_item(db, index)?;
            tracing::info!(
                code = "ESCALATION_OPEN_RACE_COALESCED",
                candidate_escalation_id = %item.escalation_id,
                winner_escalation_id = %winner.escalation_id,
                anchor = %winner.anchor,
                attention_state = %winner.attention_state,
                revision_attempt,
                "deterministic open-index CAS coalesced the generation while its projection cursor remains pending"
            );
            return Ok(Some(winner));
        }
        let expected_index_revision = prior_index.as_ref().map(|index| index.revision_sha256);
        let mut extra_rows = if item.approval_suppressed_reason.is_some() {
            GuardedExtraRows::default()
        } else {
            approval_rows_for_opened_escalation(&item, now_unix_ms)?
        };
        let approval_row_written = !extra_rows.is_empty();
        extra_rows.extend(open_index_row(&item, true, expected_index_revision)?);
        match projection_rows_for_mutation(db, transition, generation, now_unix_ms)? {
            ProjectionMutationRows::Ready(rows) => extra_rows.extend(rows),
            ProjectionMutationRows::Stale(authoritative) => {
                tracing::warn!(
                    code = "ESCALATION_TRANSITION_STALE",
                    anchor = %transition.anchor,
                    candidate_escalation_id = %item.escalation_id,
                    incoming_journal_ts_ns = generation.journal_ts_ns,
                    incoming_journal_seq = generation.journal_seq,
                    authoritative_journal_ts_ns = authoritative.observed.generation.journal_ts_ns,
                    authoritative_journal_seq = authoritative.observed.generation.journal_seq,
                    authoritative_state = %authoritative.observed.state_to,
                    "newer transition cursor prevented stale escalation creation"
                );
                return Ok(None);
            }
            ProjectionMutationRows::Applied => {
                verify_applied_projection_for_transition(db, transition, generation)?;
                return Ok(None);
            }
        }
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
                "journal_ts_ns": generation.journal_ts_ns,
                "journal_seq": generation.journal_seq,
            }),
            extra_rows,
        )?;
        match create_outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_OPENED",
                    escalation_id = %item.escalation_id,
                    anchor = %item.anchor,
                    severity = severity.as_str(),
                    attention_state = %item.attention_state,
                    tier1_eligible,
                    quiet_suppressed,
                    policy_suppressed_reason = item.tier0_suppressed_reason.as_deref(),
                    journal_ts_ns = generation.journal_ts_ns,
                    journal_seq = generation.journal_seq,
                    committed_seq,
                    watermark_key = %String::from_utf8_lossy(&projection_watermark_key(&item.anchor)),
                    "readback=CF_KV escalation, approval/index, audit, and transition watermark opened atomically"
                );
                return Ok(Some(item));
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                if let Some(authoritative) = read_projection_watermark(db, &transition.anchor)?
                    && authoritative.record.observed.generation > generation
                {
                    tracing::warn!(
                        code = "ESCALATION_TRANSITION_STALE",
                        anchor = %transition.anchor,
                        candidate_escalation_id = %item.escalation_id,
                        incoming_journal_ts_ns = generation.journal_ts_ns,
                        incoming_journal_seq = generation.journal_seq,
                        authoritative_journal_ts_ns = authoritative.record.observed.generation.journal_ts_ns,
                        authoritative_journal_seq = authoritative.record.observed.generation.journal_seq,
                        observed_seq,
                        "newer transition won the guarded escalation-create boundary; stale candidate was not written"
                    );
                    return Ok(None);
                }
                tracing::info!(
                    code = "ESCALATION_OPEN_REVISION_RETRY",
                    candidate_escalation_id = %item.escalation_id,
                    anchor = %item.anchor,
                    attention_state = %item.attention_state,
                    revision_attempt,
                    observed_seq,
                    "escalation create lost an index/watermark race; rereading both authorities"
                );
            }
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "escalation create for anchor {:?} state {:?} could not acquire stable item/index/watermark revisions after {ACK_REVISION_MAX_ATTEMPTS} attempts",
            transition.anchor,
            transition.state_to.as_str()
        ),
    ))
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
) -> Result<AckEscalationOutcome, ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| {
        let linearized_at_unix_ms =
            checked_unix_time_ms("escalation acknowledgment linearization boundary")?;
        ack_escalation_locked(db, escalation_id, via, note, linearized_at_unix_ms)
    })
    .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "escalation acknowledgment could not acquire the agent transition/Tier-0 linearization boundary: escalation_id={escalation_id} via={via:?} detail={detail}"
            ),
        )
    })?
}

fn ack_escalation_locked(
    db: &Db,
    escalation_id: &str,
    via: &str,
    note: Option<&str>,
    linearized_at_unix_ms: u64,
) -> Result<AckEscalationOutcome, ErrorData> {
    for attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let mut current = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("escalation {escalation_id} not found"),
            )
        })?;
        if current.item.status != EscalationStatus::Pending {
            // Idempotence is only honest when the linked approval and, for a
            // closed item, the deterministic open index agree with the item.
            // Older builds could commit the item alone; repair that split
            // state with the same multi-key revision guards used by the live
            // acknowledgment path.
            let existing_status = current.item.status;
            let mut repaired_item = current.item.clone();
            repaired_item.updated_at_unix_ms =
                linearized_at_unix_ms.max(repaired_item.updated_at_unix_ms);
            let mut repair_rows = linked_approval_terminal_rows(
                db,
                &repaired_item,
                "linked_escalation_ack_reconciled",
                format!(
                    "linked escalation {} was already {}; terminal approval invariant reconciled by {via}",
                    repaired_item.escalation_id,
                    existing_status.as_str()
                ),
            )?;
            if !existing_status.is_open() {
                repair_rows.extend(terminal_open_index_row(db, &repaired_item)?);
            }
            if repair_rows.is_empty() {
                verify_linked_approval_terminal(
                    db,
                    &current.item,
                    "idempotent escalation acknowledgment readback",
                )?;
                return Ok(AckEscalationOutcome {
                    escalation: current.item,
                    newly_acked: false,
                });
            }
            match write_item_and_audit_with_extra_rows_if_revision(
                db,
                &repaired_item,
                "ack_terminal_invariant_reconciled",
                json!({
                    "via": via,
                    "note": note,
                    "existing_status": existing_status.as_str(),
                }),
                repair_rows,
                current.revision_sha256,
            )? {
                ItemWriteOutcome::Applied { committed_seq, .. } => {
                    let readback = read_item(db, escalation_id)?.ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_READ_FAILED,
                            format!(
                                "reconciled escalation {escalation_id} disappeared after committed_seq={committed_seq}"
                            ),
                        )
                    })?;
                    if readback.status == EscalationStatus::Pending {
                        return Err(mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "terminal-invariant reconciliation resurrected escalation {escalation_id} as pending after committed_seq={committed_seq}"
                            ),
                        ));
                    }
                    verify_linked_approval_terminal(
                        db,
                        &readback,
                        "reconciled escalation acknowledgment readback",
                    )?;
                    tracing::warn!(
                        code = "ESCALATION_ACK_TERMINAL_INVARIANT_RECONCILED",
                        escalation_id,
                        via,
                        existing_status = existing_status.as_str(),
                        committed_seq,
                        ack_revision_attempt = attempt,
                        "readback=CF_KV repaired a pre-existing split escalation/approval/index state with one revision-guarded batch"
                    );
                    return Ok(AckEscalationOutcome {
                        escalation: readback,
                        newly_acked: false,
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
                        phase = "terminal_invariant_reconciliation",
                        "idempotent acknowledgment lost an item/approval/index revision race; rereading every authority"
                    );
                    continue;
                }
            }
        }
        let acknowledged_at_unix_ms = linearized_at_unix_ms.max(current.item.updated_at_unix_ms);
        current.item.status = EscalationStatus::Acked;
        current.item.updated_at_unix_ms = acknowledged_at_unix_ms;
        current.item.acked_at_unix_ms = Some(acknowledged_at_unix_ms);
        current.item.acked_via = Some(via.to_owned());
        current.item.next_escalate_at_unix_ms = None;
        let linked_rows = linked_approval_terminal_rows(
            db,
            &current.item,
            "linked_escalation_acked",
            format!(
                "linked escalation {} acknowledged via {via}",
                current.item.escalation_id
            ),
        )?;
        match write_item_and_audit_with_extra_rows_if_revision(
            db,
            &current.item,
            "acked",
            json!({ "via": via, "note": note }),
            linked_rows,
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
                verify_linked_approval_terminal(
                    db,
                    &readback,
                    "new escalation acknowledgment readback",
                )?;
                tracing::info!(
                    code = "ESCALATION_ACKED",
                    escalation_id,
                    via,
                    committed_seq,
                    ack_revision_attempt = attempt,
                    readback_status = readback.status.as_str(),
                    "readback=CF_KV escalation and linked approval acknowledged atomically; ladder stopped"
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
) -> Result<Option<EscalationItem>, ErrorData> {
    if approval.kind != ApprovalKind::AgentEscalation {
        return Ok(None);
    }
    let payload_json = approval.payload_json.as_deref().ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "agent escalation approval {} missing payload_json",
                approval.approval_id
            ),
        )
    })?;
    let payload = serde_json::from_str::<Value>(payload_json).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
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
                error_codes::STORAGE_CORRUPTED,
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
    let outcome = ack_escalation(db, escalation_id, &via, note)?;
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

enum WebhookDispatch {
    Send(WebhookOutboxRecord),
    Reconcile(WebhookDeliveryResult),
    NoAction,
}

fn start_webhook_dispatch(
    db: &Db,
    item: &mut EscalationItem,
    item_revision_sha256: &mut [u8; 32],
    channel: &WebhookChannel,
    receiver_generation: &str,
    config_revision_sha256: [u8; 32],
) -> Result<WebhookDispatch, ErrorData> {
    if !valid_receiver_generation(receiver_generation) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook dispatch has invalid receiver generation: channel_id={:?}",
                channel.channel_id
            ),
        ));
    }
    let ladder_index = item.ladder_index;
    if item
        .webhook_channel_ids
        .get(ladder_index as usize)
        .map(String::as_str)
        != Some(channel.channel_id.as_str())
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook dispatch channel does not match immutable escalation plan: escalation_id={} ladder_index={} planned_channel_id={:?} selected_channel_id={:?}",
                item.escalation_id,
                ladder_index,
                item.webhook_channel_ids.get(ladder_index as usize),
                channel.channel_id
            ),
        ));
    }
    let current_outbox = read_outbox_revisioned(db, &item.escalation_id, ladder_index)?;
    if let Some(current) = current_outbox {
        match current.record.state {
            WebhookOutboxState::InFlight | WebhookOutboxState::PostStarted => {
                let reconciled_at_unix_ms =
                    checked_unix_time_ms("recovered webhook outbox reconciliation boundary")?
                        .max(item.updated_at_unix_ms)
                        .max(current.record.attempt_started_at_unix_ms)
                        .max(current.record.updated_at_unix_ms);
                let post_started = current.record.state == WebhookOutboxState::PostStarted;
                let (attempt_outcome, outbox_state, error) = if post_started {
                    (
                        WebhookAttemptOutcome::Unknown,
                        WebhookOutboxState::Unknown,
                        format!(
                            "durable post-started intent was recovered without a conclusive local result; remote outcome is unknown and any retry must reuse delivery_id={}",
                            current.record.delivery_id
                        ),
                    )
                } else {
                    (
                        WebhookAttemptOutcome::TransientFailure,
                        WebhookOutboxState::TransientFailure,
                        "durable intent was recovered before POST was claimed; no remote side effect occurred and contract preflight may be retried"
                            .to_owned(),
                    )
                };
                return Ok(WebhookDispatch::Reconcile(WebhookDeliveryResult {
                    attempt: ChannelAttempt {
                        delivery_id: current.record.delivery_id.clone(),
                        channel_id: current.record.channel_id.clone(),
                        channel_name: current.record.channel_name.clone(),
                        url_host: current.record.url_host.clone(),
                        ladder_index,
                        attempt_number: current.record.attempt_number,
                        outcome: attempt_outcome,
                        ok: false,
                        http_status: current.record.http_status,
                        error: Some(error),
                        signed: current.record.signed,
                        at_unix_ms: reconciled_at_unix_ms,
                    },
                    state: outbox_state,
                    contract_verified: current.record.contract_verified,
                    response_delivery_id: current.record.response_delivery_id,
                    response_body_sha256: current.record.response_body_sha256,
                    response_receipt_state: current.record.response_receipt_state,
                }));
            }
            WebhookOutboxState::TransientFailure | WebhookOutboxState::Unknown => {
                if current.record.attempt_number >= WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "retryable webhook outbox exceeded max attempts without advancing item: escalation_id={} delivery_id={} attempt_number={} max_attempts={}",
                            item.escalation_id,
                            current.record.delivery_id,
                            current.record.attempt_number,
                            WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL
                        ),
                    ));
                }
                let mut retry = current.record;
                let retry_started_at =
                    checked_unix_time_ms("webhook retry durable-intent boundary")?
                        .max(item.updated_at_unix_ms)
                        .max(retry.attempt_started_at_unix_ms)
                        .max(retry.updated_at_unix_ms);
                retry.state = WebhookOutboxState::InFlight;
                retry.attempt_number = retry.attempt_number.saturating_add(1);
                retry.attempt_started_at_unix_ms = retry_started_at;
                retry.updated_at_unix_ms = retry_started_at;
                retry.contract_verified = false;
                retry.http_status = None;
                retry.error = None;
                retry.response_delivery_id = None;
                retry.response_body_sha256 = None;
                retry.response_receipt_state = None;
                retry.post_started_owner_epoch = None;
                item.updated_at_unix_ms = retry_started_at;
                let detail = json!({
                    "delivery_id": retry.delivery_id,
                    "channel_name": retry.channel_name,
                    "url_host": retry.url_host,
                    "ladder_index": ladder_index,
                    "attempt_number": retry.attempt_number,
                    "idempotency_contract": retry.idempotency_contract.as_str(),
                    "outbox_state": "in_flight",
                    "source_of_truth": String::from_utf8_lossy(&outbox_key(&item.escalation_id, ladder_index)),
                });
                let mut retry_rows = outbox_row(&retry, Some(current.revision_sha256))?;
                retry_rows.guards.push(RevisionGuard::new(
                    CONFIG_KEY.as_bytes().to_vec(),
                    Some(config_revision_sha256),
                ));
                let outcome = write_item_and_audit_with_extra_rows_if_revision(
                    db,
                    item,
                    "tier1_outbox_retry_started",
                    detail,
                    retry_rows,
                    *item_revision_sha256,
                )?;
                let Some(committed_seq) =
                    accept_applied_item_revision(outcome, item_revision_sha256)
                else {
                    return Ok(WebhookDispatch::NoAction);
                };
                let readback = read_outbox_revisioned(db, &item.escalation_id, ladder_index)?
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_READ_FAILED,
                            format!("webhook retry outbox {} disappeared", retry.delivery_id),
                        )
                    })?;
                if readback.record != retry {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "webhook retry outbox readback was not byte-equivalent to committed intent: delivery_id={} attempt_number={}",
                            retry.delivery_id, retry.attempt_number
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_WEBHOOK_OUTBOX_RETRY_STARTED",
                    escalation_id = %item.escalation_id,
                    delivery_id = %retry.delivery_id,
                    ladder_index,
                    attempt_number = retry.attempt_number,
                    url_host = %retry.url_host,
                    committed_seq,
                    outbox_revision = %hex_bytes(&readback.revision_sha256),
                    outbox_key = %String::from_utf8_lossy(&outbox_key(&item.escalation_id, ladder_index)),
                    "readback=CF_KV durable webhook retry intent committed before network I/O"
                );
                return Ok(WebhookDispatch::Send(readback.record));
            }
            WebhookOutboxState::Accepted
            | WebhookOutboxState::UnknownTerminal
            | WebhookOutboxState::RetryExhausted
            | WebhookOutboxState::Abandoned
            | WebhookOutboxState::TerminalFailure => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "completed webhook outbox is still due on its item: escalation_id={} delivery_id={} state={:?} item_ladder_index={}",
                        item.escalation_id,
                        current.record.delivery_id,
                        current.record.state,
                        item.ladder_index
                    ),
                ));
            }
        }
    }

    let body_json = serde_json::to_string(&webhook_payload(channel, item)).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "webhook payload serialize failed before durable intent for escalation {}: {error}",
                item.escalation_id
            ),
        )
    })?;
    let attempt_started_at_unix_ms =
        checked_unix_time_ms("webhook initial durable-intent boundary")?
            .max(item.updated_at_unix_ms);
    let outbox = WebhookOutboxRecord {
        schema_version: SCHEMA_VERSION,
        delivery_id: webhook_delivery_id(&item.escalation_id, &channel.channel_id),
        escalation_id: item.escalation_id.clone(),
        ladder_index,
        channel_id: channel.channel_id.clone(),
        channel_name: channel.name.clone(),
        url_host: webhook_url_host(&channel.url),
        idempotency_contract: channel.idempotency_contract,
        channel_fingerprint_sha256: webhook_channel_fingerprint(channel),
        receiver_generation: receiver_generation.to_owned(),
        body_sha256: hex_bytes(&Sha256::digest(body_json.as_bytes())),
        body_json,
        state: WebhookOutboxState::InFlight,
        attempt_number: 1,
        attempt_started_at_unix_ms,
        updated_at_unix_ms: attempt_started_at_unix_ms,
        contract_verified: false,
        signed: channel.secret.is_some(),
        http_status: None,
        error: None,
        response_delivery_id: None,
        response_body_sha256: None,
        response_receipt_state: None,
        post_started_owner_epoch: None,
    };
    item.updated_at_unix_ms = attempt_started_at_unix_ms;
    let detail = json!({
        "delivery_id": outbox.delivery_id,
        "channel_name": outbox.channel_name,
        "url_host": outbox.url_host,
        "ladder_index": ladder_index,
        "attempt_number": outbox.attempt_number,
        "idempotency_contract": outbox.idempotency_contract.as_str(),
        "body_sha256": outbox.body_sha256,
        "channel_fingerprint_sha256": outbox.channel_fingerprint_sha256,
        "outbox_state": "in_flight",
        "source_of_truth": String::from_utf8_lossy(&outbox_key(&item.escalation_id, ladder_index)),
    });
    let mut intent_rows = outbox_row(&outbox, None)?;
    intent_rows.guards.push(RevisionGuard::new(
        CONFIG_KEY.as_bytes().to_vec(),
        Some(config_revision_sha256),
    ));
    let outcome = write_item_and_audit_with_extra_rows_if_revision(
        db,
        item,
        "tier1_outbox_started",
        detail,
        intent_rows,
        *item_revision_sha256,
    )?;
    let Some(committed_seq) = accept_applied_item_revision(outcome, item_revision_sha256) else {
        return Ok(WebhookDispatch::NoAction);
    };
    let readback =
        read_outbox_revisioned(db, &item.escalation_id, ladder_index)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!("webhook outbox {} disappeared", outbox.delivery_id),
            )
        })?;
    if readback.record != outbox {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "webhook outbox readback was not byte-equivalent to committed intent: delivery_id={} attempt_number={}",
                outbox.delivery_id, outbox.attempt_number
            ),
        ));
    }
    tracing::info!(
        code = "ESCALATION_WEBHOOK_OUTBOX_STARTED",
        escalation_id = %item.escalation_id,
        delivery_id = %outbox.delivery_id,
        ladder_index,
        attempt_number = outbox.attempt_number,
        url_host = %outbox.url_host,
        committed_seq,
        outbox_revision = %hex_bytes(&readback.revision_sha256),
        outbox_key = %String::from_utf8_lossy(&outbox_key(&item.escalation_id, ladder_index)),
        "readback=CF_KV durable webhook intent committed before network I/O"
    );
    Ok(WebhookDispatch::Send(readback.record))
}

fn validate_webhook_delivery_transition(
    current: &WebhookOutboxRecord,
    result: &WebhookDeliveryResult,
) -> Result<(), ErrorData> {
    let identity_matches = current.delivery_id == result.attempt.delivery_id
        && current.channel_id == result.attempt.channel_id
        && current.channel_name == result.attempt.channel_name
        && current.url_host == result.attempt.url_host
        && current.ladder_index == result.attempt.ladder_index
        && current.attempt_number == result.attempt.attempt_number
        && current.signed == result.attempt.signed;
    let result_shape_matches = result.attempt.at_unix_ms >= current.attempt_started_at_unix_ms
        && result.attempt.at_unix_ms >= current.updated_at_unix_ms
        && (result.state == WebhookOutboxState::Accepted) == result.attempt.ok
        && matches!(
            (result.state, result.attempt.outcome),
            (
                WebhookOutboxState::Accepted,
                WebhookAttemptOutcome::Accepted
            ) | (
                WebhookOutboxState::TransientFailure,
                WebhookAttemptOutcome::TransientFailure
            ) | (
                WebhookOutboxState::Unknown | WebhookOutboxState::UnknownTerminal,
                WebhookAttemptOutcome::Unknown
            ) | (
                WebhookOutboxState::Abandoned,
                WebhookAttemptOutcome::Abandoned
            ) | (
                WebhookOutboxState::TerminalFailure,
                WebhookAttemptOutcome::TerminalFailure
            )
        );
    let legal_transition = match current.state {
        WebhookOutboxState::InFlight => {
            matches!(
                result.state,
                WebhookOutboxState::TransientFailure
                    | WebhookOutboxState::Abandoned
                    | WebhookOutboxState::TerminalFailure
            ) && match result.state {
                WebhookOutboxState::TransientFailure | WebhookOutboxState::TerminalFailure => {
                    !result.contract_verified
                }
                WebhookOutboxState::Abandoned => true,
                _ => false,
            }
        }
        WebhookOutboxState::PostStarted => {
            result.contract_verified
                && matches!(
                    result.state,
                    WebhookOutboxState::Accepted
                        | WebhookOutboxState::TransientFailure
                        | WebhookOutboxState::Unknown
                        | WebhookOutboxState::UnknownTerminal
                        | WebhookOutboxState::TerminalFailure
                )
        }
        WebhookOutboxState::Accepted
        | WebhookOutboxState::TransientFailure
        | WebhookOutboxState::Unknown
        | WebhookOutboxState::UnknownTerminal
        | WebhookOutboxState::RetryExhausted
        | WebhookOutboxState::Abandoned
        | WebhookOutboxState::TerminalFailure => false,
    };
    if !identity_matches || !result_shape_matches || !legal_transition {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "illegal webhook finalization transition: delivery_id={} attempt={} current_state={:?} result_state={:?} result_outcome={:?} identity_matches={} result_shape_matches={} contract_verified={} attempt_started_at={} current_updated_at={} result_at={}",
                current.delivery_id,
                current.attempt_number,
                current.state,
                result.state,
                result.attempt.outcome,
                identity_matches,
                result_shape_matches,
                result.contract_verified,
                current.attempt_started_at_unix_ms,
                current.updated_at_unix_ms,
                result.attempt.at_unix_ms
            ),
        ));
    }

    let mut candidate = current.clone();
    candidate.state = result.state;
    candidate.updated_at_unix_ms = result.attempt.at_unix_ms;
    candidate.contract_verified = result.contract_verified;
    candidate.http_status = result.attempt.http_status;
    candidate.error = result.attempt.error.clone();
    candidate.response_delivery_id = result.response_delivery_id.clone();
    candidate.response_body_sha256 = result.response_body_sha256.clone();
    candidate.response_receipt_state = result.response_receipt_state;
    candidate.post_started_owner_epoch = None;
    validate_outbox_record(
        &outbox_key(&current.escalation_id, current.ladder_index),
        &candidate,
    )
}

fn append_terminal_remediation(error: &mut Option<String>, remediation: &str) {
    match error {
        Some(previous) if !previous.contains(remediation) => {
            previous.push_str("; terminal remediation: ");
            previous.push_str(remediation);
        }
        Some(_) => {}
        None => *error = Some(format!("terminal remediation: {remediation}")),
    }
}

fn finalize_webhook_delivery(
    db: &Db,
    escalation_id: &str,
    result: &WebhookDeliveryResult,
    decision_policy_guard: Option<PolicyDecisionGuard>,
) -> Result<bool, ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let revisioned_policy = load_policy_revisioned(db)?;
        if decision_policy_guard
            .is_some_and(|guard| guard.revision_sha256 != revisioned_policy.revision_sha256)
        {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "webhook outcome {} was classified using a policy that changed before checkpoint; no stale terminal/retry decision was written and the next sweep must recompute from authoritative policy",
                    result.attempt.delivery_id
                ),
            ));
        }
        let policy = &revisioned_policy.policy;
        let current_item = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook delivery {} cannot finalize because escalation {} is missing",
                    result.attempt.delivery_id, escalation_id
                ),
            )
        })?;
        let current_outbox = read_outbox_revisioned(
            db,
            escalation_id,
            result.attempt.ladder_index,
        )?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook delivery {} cannot finalize because its durable outbox row is missing",
                    result.attempt.delivery_id
                ),
            )
        })?;
        if current_outbox.record.delivery_id != result.attempt.delivery_id
            || current_outbox.record.attempt_number != result.attempt.attempt_number
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook finalization identity mismatch: result_delivery_id={} result_attempt={} outbox_delivery_id={} outbox_attempt={}",
                    result.attempt.delivery_id,
                    result.attempt.attempt_number,
                    current_outbox.record.delivery_id,
                    current_outbox.record.attempt_number
                ),
            ));
        }
        let recorded_attempt = current_item.item.channel_attempts.iter().find(|attempt| {
            attempt.delivery_id == result.attempt.delivery_id
                && attempt.attempt_number == result.attempt.attempt_number
        });
        let active_outbox = matches!(
            current_outbox.record.state,
            WebhookOutboxState::InFlight | WebhookOutboxState::PostStarted
        );
        if !active_outbox {
            if recorded_attempt == Some(&result.attempt)
                && current_outbox.record.http_status == result.attempt.http_status
                && current_outbox.record.error == result.attempt.error
                && current_outbox.record.contract_verified == result.contract_verified
                && current_outbox.record.response_delivery_id == result.response_delivery_id
                && current_outbox.record.response_body_sha256 == result.response_body_sha256
                && current_outbox.record.response_receipt_state == result.response_receipt_state
            {
                return Ok(result.attempt.ok);
            }
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook finalization found non-active outbox without byte-equivalent item attempt: delivery_id={} outbox_state={:?} result_state={:?} recorded_attempt={recorded_attempt:?}",
                    result.attempt.delivery_id, current_outbox.record.state, result.state
                ),
            ));
        }
        if recorded_attempt.is_some() {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook attempt is present on item while outbox remains in-flight: delivery_id={} attempt_number={}",
                    result.attempt.delivery_id, result.attempt.attempt_number
                ),
            ));
        }
        validate_webhook_delivery_transition(&current_outbox.record, result)?;
        let mut item = current_item.item;
        let retryable = matches!(
            result.state,
            WebhookOutboxState::TransientFailure | WebhookOutboxState::Unknown
        );
        let pending_same_channel = item.status == EscalationStatus::Pending
            && item.ladder_index == result.attempt.ladder_index;
        let retry_backoff_ms = (retryable
            && pending_same_channel
            && result.attempt.attempt_number < WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL)
            .then(|| {
                webhook_retry_backoff_ms(
                    result.attempt.attempt_number,
                    policy.window_for(item.severity),
                )
            });
        let retry_exhausted = retryable
            && pending_same_channel
            && result.attempt.attempt_number >= WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL;
        let persisted_state = if retry_exhausted {
            if result.state == WebhookOutboxState::Unknown {
                WebhookOutboxState::UnknownTerminal
            } else {
                WebhookOutboxState::RetryExhausted
            }
        } else if retryable && !pending_same_channel {
            if result.state == WebhookOutboxState::Unknown {
                WebhookOutboxState::UnknownTerminal
            } else {
                WebhookOutboxState::TerminalFailure
            }
        } else {
            result.state
        };
        let mut persisted_attempt = result.attempt.clone();
        persisted_attempt.at_unix_ms =
            checked_unix_time_ms("webhook outcome durable-finalization boundary")?
                .max(persisted_attempt.at_unix_ms)
                .max(item.updated_at_unix_ms)
                .max(current_outbox.record.attempt_started_at_unix_ms)
                .max(current_outbox.record.updated_at_unix_ms);
        if persisted_state == WebhookOutboxState::TerminalFailure
            && result.state == WebhookOutboxState::TransientFailure
        {
            persisted_attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
            let remediation = if result.contract_verified {
                "the receiver proved not_committed, but the escalation stopped before retry; the logical delivery is terminal and no POST will be repeated"
            } else {
                "POST was never claimed and the escalation stopped before retry; the logical delivery is terminal and no network attempt will be repeated"
            };
            append_terminal_remediation(&mut persisted_attempt.error, remediation);
        }
        match persisted_state {
            WebhookOutboxState::RetryExhausted => append_terminal_remediation(
                &mut persisted_attempt.error,
                &format!(
                    "safe retry budget exhausted at {} attempts; inspect the receiver using delivery_id={} before creating a new logical delivery",
                    WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL, persisted_attempt.delivery_id
                ),
            ),
            WebhookOutboxState::UnknownTerminal => {
                let remediation = if retry_exhausted {
                    format!(
                        "remote outcome remained unknown after {} attempts; automatic delivery is stopped and the receiver must be reconciled using delivery_id={}",
                        WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL, persisted_attempt.delivery_id
                    )
                } else {
                    format!(
                        "remote outcome is not safe to retry automatically; inspect the receiver using delivery_id={} and reconcile it explicitly",
                        persisted_attempt.delivery_id
                    )
                };
                append_terminal_remediation(&mut persisted_attempt.error, &remediation);
            }
            WebhookOutboxState::Abandoned => append_terminal_remediation(
                &mut persisted_attempt.error,
                "the escalation is no longer pending on this channel; no further retry will be attempted",
            ),
            _ => {}
        }
        item.channel_attempts.push(persisted_attempt.clone());
        let completes_channel = matches!(
            persisted_state,
            WebhookOutboxState::Accepted
                | WebhookOutboxState::UnknownTerminal
                | WebhookOutboxState::RetryExhausted
                | WebhookOutboxState::Abandoned
                | WebhookOutboxState::TerminalFailure
        );
        if pending_same_channel {
            if completes_channel {
                item.ladder_index = item.ladder_index.saturating_add(1);
            }
            let policy_window_ms = policy.window_for(item.severity);
            item.next_escalate_at_unix_ms = retry_backoff_ms
                .map(|backoff| persisted_attempt.at_unix_ms.saturating_add(backoff))
                .or_else(|| {
                    ((item.ladder_index as usize) < item.webhook_channel_ids.len()).then_some(
                        persisted_attempt
                            .at_unix_ms
                            .saturating_add(policy_window_ms),
                    )
                });
        } else if item.status == EscalationStatus::Pending
            && item.ladder_index != result.attempt.ladder_index
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "pending escalation ladder moved independently of its in-flight outbox: escalation_id={} item_ladder_index={} outbox_ladder_index={} delivery_id={}",
                    item.escalation_id,
                    item.ladder_index,
                    result.attempt.ladder_index,
                    result.attempt.delivery_id
                ),
            ));
        }
        item.updated_at_unix_ms = persisted_attempt.at_unix_ms;
        let mut outbox = current_outbox.record;
        outbox.state = persisted_state;
        outbox.updated_at_unix_ms = persisted_attempt.at_unix_ms;
        outbox.contract_verified = result.contract_verified;
        outbox.http_status = persisted_attempt.http_status;
        outbox.error = persisted_attempt.error.clone();
        outbox.response_delivery_id = result.response_delivery_id.clone();
        outbox.response_body_sha256 = result.response_body_sha256.clone();
        outbox.response_receipt_state = result.response_receipt_state;
        outbox.post_started_owner_epoch = None;
        let detail = json!({
            "delivery_id": persisted_attempt.delivery_id,
            "channel_name": persisted_attempt.channel_name,
            "url_host": persisted_attempt.url_host,
            "ladder_index": persisted_attempt.ladder_index,
            "attempt_number": persisted_attempt.attempt_number,
            "max_attempts_per_channel": WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL,
            "outcome": persisted_attempt.outcome,
            "ok": persisted_attempt.ok,
            "http_status": persisted_attempt.http_status,
            "error": persisted_attempt.error,
            "contract_verified": result.contract_verified,
            "response_delivery_id": result.response_delivery_id,
            "retry_backoff_ms": retry_backoff_ms,
            "retry_exhausted": retry_exhausted,
            "outbox_state": persisted_state,
            "source_of_truth": String::from_utf8_lossy(&outbox_key(escalation_id, result.attempt.ladder_index)),
        });
        let mut finalization_rows = outbox_row(&outbox, Some(current_outbox.revision_sha256))?;
        finalization_rows.guards.push(RevisionGuard::new(
            CONFIG_KEY.as_bytes().to_vec(),
            revisioned_policy.revision_sha256,
        ));
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            "tier1_channel_attempt",
            detail,
            finalization_rows,
            current_item.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback = read_outbox_revisioned(
                    db,
                    escalation_id,
                    result.attempt.ladder_index,
                )?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "webhook outbox {} disappeared after finalization committed_seq={committed_seq}",
                            result.attempt.delivery_id
                        ),
                    )
                })?;
                if readback.record != outbox {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "webhook outbox final readback was not byte-equivalent: delivery_id={} expected_state={:?} actual_state={:?} expected_attempt={} actual_attempt={} committed_seq={committed_seq}",
                            result.attempt.delivery_id,
                            persisted_state,
                            readback.record.state,
                            result.attempt.attempt_number,
                            readback.record.attempt_number
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_WEBHOOK_OUTCOME_DURABLE",
                    escalation_id,
                    delivery_id = %result.attempt.delivery_id,
                    ladder_index = result.attempt.ladder_index,
                    attempt_number = result.attempt.attempt_number,
                    outcome = ?result.attempt.outcome,
                    outbox_state = ?persisted_state,
                    contract_verified = result.contract_verified,
                    url_host = %result.attempt.url_host,
                    committed_seq,
                    outbox_key = %String::from_utf8_lossy(&outbox_key(escalation_id, result.attempt.ladder_index)),
                    "readback=CF_KV webhook outcome durably reconciled to outbox, item, and audit"
                );
                return Ok(result.attempt.ok);
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                if let Some(guard) = decision_policy_guard
                    && load_policy_revisioned(db)?.revision_sha256 != guard.revision_sha256
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_WRITE_FAILED,
                        format!(
                            "webhook outcome {} lost its policy decision guard during finalization; no stale decision was written and recovery must be recomputed",
                            result.attempt.delivery_id
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_WEBHOOK_FINALIZE_REVISION_RETRY",
                    escalation_id,
                    delivery_id = %result.attempt.delivery_id,
                    attempt_number = result.attempt.attempt_number,
                    revision_attempt,
                    observed_seq,
                    "webhook outcome raced an item mutation; rereading item and outbox without repeating network I/O"
                );
            }
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "webhook outcome {} could not acquire stable item/outbox revisions after {ACK_REVISION_MAX_ATTEMPTS} attempts; the durable in-flight intent remains authoritative and must be reconciled before retry",
            result.attempt.delivery_id
        ),
    ))
}

fn reconcile_recovered_inflight_outbox(
    db: &Db,
    item: &EscalationItem,
    now_unix_ms: u64,
) -> Result<bool, ErrorData> {
    let Some(current) = read_outbox_revisioned(db, &item.escalation_id, item.ladder_index)? else {
        return Ok(false);
    };
    if !matches!(
        current.record.state,
        WebhookOutboxState::InFlight | WebhookOutboxState::PostStarted
    ) {
        return Ok(false);
    }
    let post_started = current.record.state == WebhookOutboxState::PostStarted;
    let revisioned_policy = load_policy_revisioned(db)?;
    let policy = &revisioned_policy.policy;
    let same_receiver = policy
        .webhooks
        .iter()
        .find(|channel| channel.channel_id == current.record.channel_id)
        .is_some_and(|channel| {
            channel.name == current.record.channel_name
                && channel.idempotency_contract == current.record.idempotency_contract
                && webhook_url_host(&channel.url) == current.record.url_host
                && webhook_channel_fingerprint(channel) == current.record.channel_fingerprint_sha256
                && channel.secret.is_some() == current.record.signed
                && policy
                    .receiver_generations
                    .get(&channel.channel_id)
                    .is_some_and(|generation| generation == &current.record.receiver_generation)
        });
    let state = if post_started && same_receiver {
        WebhookOutboxState::Unknown
    } else if post_started {
        WebhookOutboxState::UnknownTerminal
    } else if same_receiver {
        WebhookOutboxState::TransientFailure
    } else {
        WebhookOutboxState::Abandoned
    };
    let retry_remediation = if post_started && same_receiver {
        format!(
            "recovered durable post-started intent without a conclusive local result; remote outcome is unknown and any retry must reuse delivery_id={}",
            current.record.delivery_id
        )
    } else if post_started {
        format!(
            "recovered durable post-started intent after receiver configuration changed or disappeared; remote outcome is unknown and retry is disabled for delivery_id={}",
            current.record.delivery_id
        )
    } else if same_receiver {
        "recovered durable preflight intent before POST was claimed; no remote side effect occurred and preflight may be retried"
            .to_owned()
    } else {
        "recovered durable preflight intent after receiver configuration changed or disappeared; no POST occurred and retry was abandoned"
            .to_owned()
    };
    let result = WebhookDeliveryResult {
        attempt: ChannelAttempt {
            delivery_id: current.record.delivery_id.clone(),
            channel_id: current.record.channel_id.clone(),
            channel_name: current.record.channel_name.clone(),
            url_host: current.record.url_host.clone(),
            ladder_index: current.record.ladder_index,
            attempt_number: current.record.attempt_number,
            outcome: match state {
                WebhookOutboxState::Unknown | WebhookOutboxState::UnknownTerminal => {
                    WebhookAttemptOutcome::Unknown
                }
                WebhookOutboxState::TransientFailure => WebhookAttemptOutcome::TransientFailure,
                WebhookOutboxState::Abandoned => WebhookAttemptOutcome::Abandoned,
                _ => {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "recovered active webhook produced illegal state: delivery_id={} state={state:?}",
                            current.record.delivery_id
                        ),
                    ));
                }
            },
            ok: false,
            http_status: current.record.http_status,
            error: Some(retry_remediation),
            signed: current.record.signed,
            at_unix_ms: now_unix_ms
                .max(item.updated_at_unix_ms)
                .max(current.record.attempt_started_at_unix_ms)
                .max(current.record.updated_at_unix_ms),
        },
        state,
        contract_verified: current.record.contract_verified,
        response_delivery_id: current.record.response_delivery_id,
        response_body_sha256: current.record.response_body_sha256,
        response_receipt_state: current.record.response_receipt_state,
    };
    tracing::warn!(
        code = "ESCALATION_WEBHOOK_INFLIGHT_RECOVERED_UNKNOWN",
        escalation_id = %item.escalation_id,
        delivery_id = %result.attempt.delivery_id,
        ladder_index = result.attempt.ladder_index,
        attempt_number = result.attempt.attempt_number,
        item_status = item.status.as_str(),
        same_receiver,
        post_started,
        outcome_state = ?state,
        "recovered in-flight outbox is being checkpointed before status-specific worker handling"
    );
    let _ = finalize_webhook_delivery(
        db,
        &item.escalation_id,
        &result,
        Some(PolicyDecisionGuard {
            revision_sha256: revisioned_policy.revision_sha256,
        }),
    )?;
    Ok(true)
}

fn stopped_retryable_terminal_record(
    mut item: EscalationItem,
    current: &RevisionedWebhookOutbox,
    now_unix_ms: u64,
) -> Result<Option<(EscalationItem, WebhookOutboxRecord, String)>, ErrorData> {
    if item.status == EscalationStatus::Pending {
        return Ok(None);
    }
    let (terminal_state, terminal_outcome, remediation) = match current.record.state {
        WebhookOutboxState::Unknown => (
            WebhookOutboxState::UnknownTerminal,
            WebhookAttemptOutcome::Unknown,
            format!(
                "escalation status {} stopped delivery; the remote outcome remains unknown and the receiver must be reconciled using delivery_id={}",
                item.status.as_str(),
                current.record.delivery_id
            ),
        ),
        WebhookOutboxState::TransientFailure => (
            WebhookOutboxState::TerminalFailure,
            WebhookAttemptOutcome::TerminalFailure,
            if current.record.contract_verified {
                "the receiver proved not_committed, but the escalation stopped before retry; the logical delivery is terminal and no POST will be repeated".to_owned()
            } else {
                "POST was never claimed and the escalation stopped before retry; the logical delivery is terminal and no network attempt will be repeated".to_owned()
            },
        ),
        WebhookOutboxState::InFlight | WebhookOutboxState::PostStarted => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "active webhook must be reconciled before stopped-delivery terminalization: escalation_id={} delivery_id={} state={:?}",
                    item.escalation_id, current.record.delivery_id, current.record.state
                ),
            ));
        }
        WebhookOutboxState::Accepted
        | WebhookOutboxState::UnknownTerminal
        | WebhookOutboxState::RetryExhausted
        | WebhookOutboxState::Abandoned
        | WebhookOutboxState::TerminalFailure => return Ok(None),
    };
    let attempt = item
        .channel_attempts
        .iter_mut()
        .find(|attempt| {
            attempt.delivery_id == current.record.delivery_id
                && attempt.attempt_number == current.record.attempt_number
        })
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "retryable webhook outbox has no matching item attempt: escalation_id={} delivery_id={} attempt_number={}",
                    item.escalation_id,
                    current.record.delivery_id,
                    current.record.attempt_number
                ),
            )
        })?;
    let expected_outcome = match current.record.state {
        WebhookOutboxState::Unknown => WebhookAttemptOutcome::Unknown,
        WebhookOutboxState::TransientFailure => WebhookAttemptOutcome::TransientFailure,
        _ => unreachable!("retryable state was matched above"),
    };
    if attempt.channel_id != current.record.channel_id
        || attempt.channel_name != current.record.channel_name
        || attempt.url_host != current.record.url_host
        || attempt.ladder_index != current.record.ladder_index
        || attempt.outcome != expected_outcome
        || attempt.ok
        || attempt.http_status != current.record.http_status
        || attempt.error != current.record.error
        || attempt.signed != current.record.signed
        || attempt.at_unix_ms != current.record.updated_at_unix_ms
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "retryable webhook item/outbox evidence differs before terminalization: escalation_id={} delivery_id={} attempt_number={} outbox_state={:?}",
                item.escalation_id,
                current.record.delivery_id,
                current.record.attempt_number,
                current.record.state
            ),
        ));
    }
    let terminalized_at = now_unix_ms
        .max(item.updated_at_unix_ms)
        .max(current.record.updated_at_unix_ms);
    attempt.outcome = terminal_outcome;
    attempt.at_unix_ms = terminalized_at;
    append_terminal_remediation(&mut attempt.error, &remediation);
    let mut outbox = current.record.clone();
    outbox.state = terminal_state;
    outbox.updated_at_unix_ms = terminalized_at;
    outbox.error.clone_from(&attempt.error);
    item.updated_at_unix_ms = terminalized_at;
    Ok(Some((item, outbox, remediation)))
}

fn terminalize_stopped_retryable_outbox(
    db: &Db,
    escalation_id: &str,
    ladder_index: u32,
    now_unix_ms: u64,
) -> Result<bool, ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let current_item = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "stopped webhook cannot be terminalized because escalation {escalation_id} is missing"
                ),
            )
        })?;
        let Some(current_outbox) = read_outbox_revisioned(db, escalation_id, ladder_index)? else {
            return Ok(false);
        };
        let Some((item, outbox, remediation)) =
            stopped_retryable_terminal_record(current_item.item, &current_outbox, now_unix_ms)?
        else {
            return Ok(false);
        };
        let terminal_state = outbox.state;
        let rows = outbox_row(&outbox, Some(current_outbox.revision_sha256))?;
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            "tier1_outbox_stopped",
            json!({
                "delivery_id": outbox.delivery_id,
                "ladder_index": ladder_index,
                "attempt_number": outbox.attempt_number,
                "outbox_state": terminal_state,
                "error": remediation,
                "source_of_truth": String::from_utf8_lossy(&outbox_key(escalation_id, ladder_index)),
            }),
            rows,
            current_item.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback = read_outbox_revisioned(db, escalation_id, ladder_index)?
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_READ_FAILED,
                            format!(
                                "stopped webhook outbox {} disappeared after terminalization committed_seq={committed_seq}",
                                outbox.delivery_id
                            ),
                        )
                    })?;
                if readback.record != outbox {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "stopped webhook terminal readback differed: delivery_id={} expected_state={terminal_state:?} actual_state={:?} committed_seq={committed_seq}",
                            outbox.delivery_id, readback.record.state
                        ),
                    ));
                }
                tracing::warn!(
                    code = "ESCALATION_WEBHOOK_OUTBOX_STOPPED",
                    escalation_id,
                    delivery_id = %outbox.delivery_id,
                    ladder_index,
                    attempt_number = outbox.attempt_number,
                    state = ?terminal_state,
                    committed_seq,
                    "readback=CF_KV stopped escalation retry/unknown webhook evidence was atomically made terminal before retention"
                );
                return Ok(true);
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                code = "ESCALATION_WEBHOOK_STOPPED_REVISION_RETRY",
                escalation_id,
                ladder_index,
                revision_attempt,
                observed_seq,
                "stopped webhook terminalization lost a revision race; rereading item and outbox"
            ),
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "stopped webhook escalation {escalation_id} ladder_index={ladder_index} could not acquire stable revisions after {ACK_REVISION_MAX_ATTEMPTS} attempts"
        ),
    ))
}

fn reconcile_stopped_webhook_outboxes(
    db: &Db,
    item: &EscalationItem,
    now_unix_ms: u64,
) -> Result<usize, ErrorData> {
    let current = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "stopped webhook reconciliation cannot find escalation {}",
                item.escalation_id
            ),
        )
    })?;
    if current.item.status == EscalationStatus::Pending {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "stopped webhook reconciliation was requested for pending escalation {}",
                item.escalation_id
            ),
        ));
    }
    let mut terminalized = usize::from(reconcile_recovered_inflight_outbox(
        db,
        &current.item,
        now_unix_ms,
    )?);
    for ladder_index in 0..MAX_WEBHOOKS as u32 {
        terminalized += usize::from(terminalize_stopped_retryable_outbox(
            db,
            &item.escalation_id,
            ladder_index,
            now_unix_ms,
        )?);
    }
    Ok(terminalized)
}

fn terminalize_deconfigured_outbox(
    db: &Db,
    escalation_id: &str,
    ladder_index: u32,
    now_unix_ms: u64,
) -> Result<(), ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let revisioned_policy = load_policy_revisioned(db)?;
        let current_item = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "deconfigured webhook outbox cannot reconcile because escalation {escalation_id} is missing"
                ),
            )
        })?;
        let current_outbox = read_outbox_revisioned(db, escalation_id, ladder_index)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "deconfigured webhook outbox disappeared: escalation_id={escalation_id} ladder_index={ladder_index}"
                    ),
                )
            })?;
        let same_receiver = revisioned_policy
            .policy
            .webhooks
            .iter()
            .find(|channel| channel.channel_id == current_outbox.record.channel_id)
            .is_some_and(|channel| {
                channel.name == current_outbox.record.channel_name
                    && channel.idempotency_contract == current_outbox.record.idempotency_contract
                    && webhook_url_host(&channel.url) == current_outbox.record.url_host
                    && webhook_channel_fingerprint(channel)
                        == current_outbox.record.channel_fingerprint_sha256
                    && channel.secret.is_some() == current_outbox.record.signed
                    && revisioned_policy
                        .policy
                        .receiver_generations
                        .get(&channel.channel_id)
                        .is_some_and(|generation| {
                            generation == &current_outbox.record.receiver_generation
                        })
            });
        if same_receiver {
            return Ok(());
        }
        let terminal_state = match current_outbox.record.state {
            WebhookOutboxState::Unknown => WebhookOutboxState::UnknownTerminal,
            WebhookOutboxState::TransientFailure => WebhookOutboxState::TerminalFailure,
            WebhookOutboxState::InFlight | WebhookOutboxState::PostStarted => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "in-flight webhook {} must be checkpointed as an explicit unknown outcome before policy-removal terminalization",
                        current_outbox.record.delivery_id
                    ),
                ));
            }
            WebhookOutboxState::Accepted
            | WebhookOutboxState::UnknownTerminal
            | WebhookOutboxState::RetryExhausted
            | WebhookOutboxState::Abandoned
            | WebhookOutboxState::TerminalFailure => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "completed webhook outbox remains due after policy removal: delivery_id={} state={:?}",
                        current_outbox.record.delivery_id, current_outbox.record.state
                    ),
                ));
            }
        };
        let mut item = current_item.item;
        if item.status != EscalationStatus::Pending || item.ladder_index != ladder_index {
            return Ok(());
        }
        item.ladder_index = item.ladder_index.saturating_add(1);
        item.next_escalate_at_unix_ms = None;
        let terminalized_at = now_unix_ms
            .max(item.updated_at_unix_ms)
            .max(current_outbox.record.attempt_started_at_unix_ms)
            .max(current_outbox.record.updated_at_unix_ms);
        item.updated_at_unix_ms = terminalized_at;
        let mut outbox = current_outbox.record;
        outbox.state = terminal_state;
        outbox.updated_at_unix_ms = terminalized_at;
        let remediation = format!(
            "webhook channel at ladder index {ladder_index} was removed after attempt {}; retry disabled because the original endpoint identity/secret is no longer configured",
            outbox.attempt_number
        );
        outbox.error = Some(match outbox.error.take() {
            Some(previous) => format!("{previous}; {remediation}"),
            None => remediation.clone(),
        });
        let mut terminal_rows = outbox_row(&outbox, Some(current_outbox.revision_sha256))?;
        terminal_rows.guards.push(RevisionGuard::new(
            CONFIG_KEY.as_bytes().to_vec(),
            revisioned_policy.revision_sha256,
        ));
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            "tier1_outbox_deconfigured",
            json!({
                "delivery_id": outbox.delivery_id,
                "ladder_index": ladder_index,
                "attempt_number": outbox.attempt_number,
                "outbox_state": terminal_state,
                "error": remediation,
                "source_of_truth": String::from_utf8_lossy(&outbox_key(escalation_id, ladder_index)),
            }),
            terminal_rows,
            current_item.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback = read_outbox_revisioned(db, escalation_id, ladder_index)?
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_READ_FAILED,
                            format!(
                                "deconfigured webhook outbox {} disappeared after terminalization committed_seq={committed_seq}",
                                outbox.delivery_id
                            ),
                        )
                    })?;
                if readback.record != outbox {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "deconfigured webhook outbox terminal readback differed: delivery_id={} expected_state={terminal_state:?} actual_state={:?} committed_seq={committed_seq}",
                            outbox.delivery_id, readback.record.state
                        ),
                    ));
                }
                tracing::warn!(
                    code = "ESCALATION_WEBHOOK_OUTBOX_DECONFIGURED",
                    escalation_id,
                    delivery_id = %outbox.delivery_id,
                    ladder_index,
                    attempt_number = outbox.attempt_number,
                    state = ?terminal_state,
                    committed_seq,
                    "readback=CF_KV removed webhook channel cannot be retried; outbox and item were terminalized atomically"
                );
                return Ok(());
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_WEBHOOK_DECONFIGURE_REVISION_RETRY",
                    escalation_id,
                    ladder_index,
                    revision_attempt,
                    observed_seq,
                    "webhook policy-removal reconciliation raced another mutation; rereading"
                );
            }
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "deconfigured webhook outbox for escalation {escalation_id} could not acquire stable revisions after {ACK_REVISION_MAX_ATTEMPTS} attempts"
        ),
    ))
}

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
    let Some(_reconciled) =
        reconcile_transition_projections_cancellable(db.as_ref(), shutdown).await?
    else {
        tracing::info!(
            code = "ESCALATION_WORKER_STOPPED",
            stage = "during_pending_projection_pages",
            elapsed_ms = sweep_started.elapsed().as_millis(),
            "stopping escalation worker at the Pending-projection page cancellation boundary"
        );
        return Ok(None);
    };
    if shutdown.is_cancelled() {
        return Ok(None);
    }
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
            let terminalized = reconcile_stopped_webhook_outboxes(db, &item, now_unix_ms)?;
            report.tier1_failed += terminalized;
            if terminalized > 0 {
                let Some(reconciled) = read_item_revisioned(db, &item.escalation_id)? else {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "terminal escalation {} disappeared after stopped webhook reconciliation",
                            item.escalation_id
                        ),
                    ));
                };
                item = reconciled.item;
                item_revision_sha256 = reconciled.revision_sha256;
            }
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
                    item.updated_at_unix_ms =
                        checked_unix_time_ms("terminal linked-approval closure boundary")?
                            .max(now_unix_ms)
                            .max(item.updated_at_unix_ms);
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
        let authoritative_agent_reads = super::agent_state::reads(now_unix_ms);
        let agent_state = authoritative_agent_read_for_item(&authoritative_agent_reads, &item)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "open escalation has no authoritative agent-state projection: escalation_id={} anchor={:?}; refusing toast/webhook side effects",
                        item.escalation_id, item.anchor
                    ),
                )
            })?;
        if agent_state.state == AgentLifecycleState::Dead
            || agent_state.state.as_str() != item.attention_state
        {
            item.status = EscalationStatus::Resolved;
            item.updated_at_unix_ms =
                checked_unix_time_ms("authoritative-state escalation resolution boundary")?
                    .max(now_unix_ms)
                    .max(item.updated_at_unix_ms);
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
            let terminalized = reconcile_stopped_webhook_outboxes(db, &item, now_unix_ms)?;
            report.tier1_failed += terminalized;
            if terminalized > 0 {
                let reconciled = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "resolved escalation {} disappeared after stopped webhook reconciliation",
                            item.escalation_id
                        ),
                    )
                })?;
                item = reconciled.item;
                item_revision_sha256 = reconciled.revision_sha256;
            }
            if remove_tier0_if_terminal(db, &mut item, &mut item_revision_sha256).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            continue;
        }
        if item.status == EscalationStatus::Acked {
            let terminalized = reconcile_stopped_webhook_outboxes(db, &item, now_unix_ms)?;
            report.tier1_failed += terminalized;
            if terminalized > 0 {
                let Some(reconciled) = read_item_revisioned(db, &item.escalation_id)? else {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "acked escalation {} disappeared after stopped webhook reconciliation",
                            item.escalation_id
                        ),
                    ));
                };
                item = reconciled.item;
                item_revision_sha256 = reconciled.revision_sha256;
            }
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
            item.updated_at_unix_ms =
                checked_unix_time_ms("escalation TTL terminalization boundary")?
                    .max(now_unix_ms)
                    .max(item.updated_at_unix_ms);
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
            let terminalized = reconcile_stopped_webhook_outboxes(db, &item, now_unix_ms)?;
            report.tier1_failed += terminalized;
            if terminalized > 0 {
                let reconciled = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "expired escalation {} disappeared after stopped webhook reconciliation",
                            item.escalation_id
                        ),
                    )
                })?;
                item = reconciled.item;
                item_revision_sha256 = reconciled.revision_sha256;
            }
            if remove_tier0_if_terminal(db, &mut item, &mut item_revision_sha256).await? {
                report.tier0_removed += 1;
            } else if tier0_removal_failed(&item) {
                report.tier0_remove_failed += 1;
            }
            continue;
        }

        // Only reconcile an unfinished network intent after authoritative
        // agent-state and TTL checks have proved this escalation generation is
        // still live. Recovery must never advance a stale delivery ahead of the
        // local source of truth that can close it.
        if reconcile_recovered_inflight_outbox(db, &item, now_unix_ms)? {
            report.tier1_failed += 1;
            let Some(reconciled) = read_item_revisioned(db, &item.escalation_id)? else {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "escalation {} disappeared after recovered in-flight webhook reconciliation",
                        item.escalation_id
                    ),
                ));
            };
            item = reconciled.item;
            item_revision_sha256 = reconciled.revision_sha256;
        }

        let mut dirty = false;

        // Tier 0 — on-PC toast (always, regardless of egress config).
        if item.tier0_suppressed_reason.is_none() {
            match drive_tier0_delivery(db, &item.escalation_id, now_unix_ms).await? {
                Tier0ProcessOutcome::Fired => report.tier0_fired += 1,
                Tier0ProcessOutcome::Busy => continue,
                Tier0ProcessOutcome::ExpiredBeforeShow => {
                    tracing::info!(
                        code = "ESCALATION_TIER0_EXPIRED_BEFORE_SHOW",
                        escalation_id = %item.escalation_id,
                        "WinRT refused Show at the exact deadline boundary; deferring to fresh TTL terminalization without Tier-1 side effects"
                    );
                    continue;
                }
                Tier0ProcessOutcome::NoAction => {}
            }
            let latest = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "escalation {} disappeared after Tier-0 delivery processing",
                        item.escalation_id
                    ),
                )
            })?;
            item = latest.item;
            item_revision_sha256 = latest.revision_sha256;
            if item.status != EscalationStatus::Pending {
                report.tier1_failed += reconcile_stopped_webhook_outboxes(db, &item, now_unix_ms)?;
                let refreshed = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "stopped escalation {} disappeared after Tier-0 race reconciliation",
                            item.escalation_id
                        ),
                    )
                })?;
                item = refreshed.item;
                item_revision_sha256 = refreshed.revision_sha256;
                if remove_tier0_if_terminal(db, &mut item, &mut item_revision_sha256).await? {
                    report.tier0_removed += 1;
                } else if tier0_removal_failed(&item) {
                    report.tier0_remove_failed += 1;
                }
                continue;
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
            if shutdown.is_cancelled() {
                return Ok(None);
            }
            let revisioned_policy = load_policy_revisioned(db)?;
            let config_revision_sha256 = revisioned_policy.revision_sha256.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "tier1-eligible escalation has no physical policy revision: escalation_id={}",
                        item.escalation_id
                    ),
                )
            })?;
            let index = item.ladder_index as usize;
            let planned_channel_id = item.webhook_channel_ids.get(index);
            let channel = planned_channel_id.and_then(|channel_id| {
                revisioned_policy
                    .policy
                    .webhooks
                    .iter()
                    .find(|channel| &channel.channel_id == channel_id)
                    .cloned()
            });
            if let Some(channel) = channel {
                let receiver_generation = revisioned_policy
                    .policy
                    .receiver_generations
                    .get(&channel.channel_id)
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "tier1 webhook channel has no receiver generation: escalation_id={} channel_id={:?}",
                                item.escalation_id, channel.channel_id
                            ),
                        )
                    })?;
                if let Some(existing) =
                    read_outbox_revisioned(db, &item.escalation_id, item.ladder_index)?
                {
                    let same_receiver = existing.record.receiver_generation == *receiver_generation
                        && existing.record.channel_fingerprint_sha256
                            == webhook_channel_fingerprint(&channel)
                        && existing.record.signed == channel.secret.is_some();
                    if !same_receiver {
                        if !matches!(
                            existing.record.state,
                            WebhookOutboxState::Unknown | WebhookOutboxState::TransientFailure
                        ) {
                            return Err(mcp_error(
                                error_codes::STORAGE_CORRUPTED,
                                format!(
                                    "receiver generation changed while a non-retryable outbox remained due: escalation_id={} delivery_id={} state={:?}",
                                    item.escalation_id,
                                    existing.record.delivery_id,
                                    existing.record.state
                                ),
                            ));
                        }
                        terminalize_deconfigured_outbox(
                            db,
                            &item.escalation_id,
                            item.ladder_index,
                            now_unix_ms,
                        )?;
                        report.tier1_failed += 1;
                        continue;
                    }
                }
                let dispatch = start_webhook_dispatch(
                    db,
                    &mut item,
                    &mut item_revision_sha256,
                    &channel,
                    receiver_generation,
                    config_revision_sha256,
                )?;
                let result = match dispatch {
                    WebhookDispatch::Send(outbox) => {
                        tracing::info!(
                            code = "ESCALATION_WEBHOOK_NETWORK_STARTED",
                            escalation_id = %item.escalation_id,
                            delivery_id = %outbox.delivery_id,
                            ladder_index = outbox.ladder_index,
                            attempt_number = outbox.attempt_number,
                            url_host = %outbox.url_host,
                            timeout_ms = WEBHOOK_TIMEOUT_MS,
                            "durable outbox readback succeeded; starting contract preflight and POST"
                        );
                        let Some(result) = deliver_webhook(db, &channel, &outbox, shutdown).await?
                        else {
                            continue;
                        };
                        result
                    }
                    WebhookDispatch::Reconcile(result) => {
                        tracing::warn!(
                            code = "ESCALATION_WEBHOOK_INFLIGHT_RECOVERED_UNKNOWN",
                            escalation_id = %item.escalation_id,
                            delivery_id = %result.attempt.delivery_id,
                            ladder_index = result.attempt.ladder_index,
                            attempt_number = result.attempt.attempt_number,
                            url_host = %result.attempt.url_host,
                            "recovered in-flight outbox has an ambiguous remote outcome; checkpointing unknown before any retry"
                        );
                        result
                    }
                    WebhookDispatch::NoAction => continue,
                };
                if finalize_webhook_delivery(db, &item.escalation_id, &result, None)? {
                    report.tier1_fired += 1;
                } else {
                    report.tier1_failed += 1;
                }
                dirty = false;
            } else {
                // Configuration can shrink while an intent is active. The
                // central recovery path preserves the decisive distinction:
                // InFlight is known-unsent and abandoned, while PostStarted is
                // a possible remote side effect and becomes unknown-terminal.
                let ladder_index = item.ladder_index;
                match read_outbox_revisioned(db, &item.escalation_id, ladder_index)? {
                    Some(current)
                        if matches!(
                            current.record.state,
                            WebhookOutboxState::InFlight | WebhookOutboxState::PostStarted
                        ) =>
                    {
                        if !reconcile_recovered_inflight_outbox(db, &item, now_unix_ms)? {
                            return Err(mcp_error(
                                error_codes::STORAGE_CORRUPTED,
                                format!(
                                    "active webhook outbox disappeared before central reconciliation: escalation_id={} delivery_id={} state={:?}",
                                    item.escalation_id,
                                    current.record.delivery_id,
                                    current.record.state
                                ),
                            ));
                        }
                        report.tier1_failed += 1;
                        // Central reconciliation atomically advanced or
                        // rescheduled the authoritative item. Do not apply the
                        // stale local copy later in this sweep.
                        continue;
                    }
                    Some(current)
                        if matches!(
                            current.record.state,
                            WebhookOutboxState::Unknown | WebhookOutboxState::TransientFailure
                        ) =>
                    {
                        terminalize_deconfigured_outbox(
                            db,
                            &item.escalation_id,
                            ladder_index,
                            now_unix_ms,
                        )?;
                        report.tier1_failed += 1;
                        dirty = false;
                    }
                    Some(current) => {
                        return Err(mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "completed webhook outbox is still due after channel removal: escalation_id={} delivery_id={} state={:?}",
                                item.escalation_id,
                                current.record.delivery_id,
                                current.record.state
                            ),
                        ));
                    }
                    None => {
                        item.next_escalate_at_unix_ms = None;
                        dirty = true;
                    }
                }
            }
        }

        if dirty {
            item.updated_at_unix_ms =
                checked_unix_time_ms("escalation worker dirty-item mutation boundary")?
                    .max(now_unix_ms)
                    .max(item.updated_at_unix_ms);
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

async fn migrate_tier0_payload_v1_if_present(
    db: &Db,
    item: &mut EscalationItem,
    item_revision_sha256: &mut [u8; 32],
) -> Result<(), ErrorData> {
    for migration_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let Some(legacy_payload) = item.tier0_prepared_payload.clone() else {
            return Ok(());
        };
        if prepared_toast_payload_valid(&legacy_payload) {
            if !tier0_prepared_request_binding_valid(item, &legacy_payload) {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Tier-0 schema-v2 payload is not bound to its durable escalation request: escalation_id={} payload_sha256={} intent_sha256={}",
                        item.escalation_id,
                        legacy_payload.payload_sha256,
                        legacy_payload.intent_sha256
                    ),
                ));
            }
            return Ok(());
        }
        if !legacy_prepared_toast_payload_v1_valid(&legacy_payload) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 frozen payload is neither valid schema v2 nor an exact migratable schema v1 payload: escalation_id={} schema_version={} payload_sha256={:?} intent_sha256={:?}",
                    item.escalation_id,
                    legacy_payload.schema_version,
                    legacy_payload.payload_sha256,
                    legacy_payload.intent_sha256
                ),
            ));
        }
        let uses_legacy_identity = item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified
            || tier0_delivery_tag(&item.tier0_delivery)
                == Some(legacy_escalation_toast_tag(&item.escalation_id).as_str());
        let logical_params = if uses_legacy_identity {
            legacy_tier0_notify_params(item)
        } else {
            tier0_notify_params_v1(item)
        };
        let canonical_probe =
            prepare_internal_escalation_toast(logical_params.clone(), Vec::new()).await?;
        let Some(upgraded_payload) = upgrade_prepared_toast_payload_v1(
            &legacy_payload,
            &canonical_probe,
            &logical_params,
            &[],
        ) else {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 schema-v1 payload cannot be migrated because the frozen v1 renderer's fresh WinRT canonical readback differs: escalation_id={} legacy_payload_sha256={} rendered_payload_sha256={} canonical_xml_equal={} suppress_popup_equal={} logical_identity={} legacy_renderer_version={}",
                    item.escalation_id,
                    legacy_payload.payload_sha256,
                    canonical_probe.payload_sha256,
                    legacy_payload.canonical_xml == canonical_probe.canonical_xml,
                    legacy_payload.suppress_popup == canonical_probe.suppress_popup,
                    if uses_legacy_identity {
                        "legacy_tag_group_template"
                    } else {
                        "reserved_tag_group_template"
                    },
                    TOAST_RENDERER_VERSION_V1,
                ),
            ));
        };
        let expected_revision = *item_revision_sha256;
        let outcome = super::agent_state::with_transition_pipeline_lock(|| {
            let migration_now =
                checked_unix_time_ms("Tier-0 payload v1-to-v2 migration boundary")?
                    .max(item.updated_at_unix_ms);
            let mut updated = item.clone();
            updated.tier0_prepared_payload = Some(upgraded_payload.clone());
            updated.updated_at_unix_ms = migration_now;
            write_item_and_audit_if_revision(
                db,
                &updated,
                "tier0_payload_intent_binding_migrated_v1_to_v2",
                json!({
                    "old_schema_version": legacy_payload.schema_version,
                    "new_schema_version": upgraded_payload.schema_version,
                    "renderer_version": upgraded_payload.renderer_version,
                    "payload_sha256": upgraded_payload.payload_sha256,
                    "intent_sha256": upgraded_payload.intent_sha256,
                    "canonical_xml_exact_match": true,
                    "suppress_popup_exact_match": true,
                    "logical_identity": if uses_legacy_identity {
                        "legacy_tag_group_template"
                    } else {
                        "reserved_tag_group_template"
                    },
                    "renderer": "WinRT XmlDocument.LoadXml/GetXml",
                }),
                expected_revision,
            )
        })
        .map_err(|detail| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "Tier-0 payload migration could not acquire the transition boundary: escalation_id={} detail={detail}",
                    item.escalation_id
                ),
            )
        })??;
        match outcome {
            ItemWriteOutcome::Applied {
                revision_sha256,
                committed_seq,
            } => {
                let readback = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "Tier-0 payload migration item {} disappeared after committed_seq={committed_seq}",
                            item.escalation_id
                        ),
                    )
                })?;
                if readback.item.tier0_prepared_payload.as_ref() != Some(&upgraded_payload)
                    || readback.item.tier0_payload_sha256
                        != Some(upgraded_payload.payload_sha256.clone())
                    || readback.revision_sha256 != revision_sha256
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 payload v1-to-v2 migration readback differed: escalation_id={} expected_payload_sha256={} expected_intent_sha256={} committed_seq={committed_seq}",
                            item.escalation_id,
                            upgraded_payload.payload_sha256,
                            upgraded_payload.intent_sha256
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_TIER0_PAYLOAD_V2_MIGRATED",
                    escalation_id = %item.escalation_id,
                    migration_attempt,
                    committed_seq,
                    payload_sha256 = %upgraded_payload.payload_sha256,
                    intent_sha256 = %upgraded_payload.intent_sha256,
                    "readback=CF_KV schema-v2 frozen payload preserves exact schema-v1 XML while binding the durable logical request"
                );
                *item = readback.item;
                *item_revision_sha256 = readback.revision_sha256;
                return Ok(());
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_TIER0_PAYLOAD_V2_MIGRATION_RETRY",
                    escalation_id = %item.escalation_id,
                    migration_attempt,
                    observed_seq,
                    "Tier-0 payload migration raced another item transition; rereading and rerendering"
                );
                let refreshed = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "Tier-0 payload migration lost escalation item {} after a revision conflict",
                            item.escalation_id
                        ),
                    )
                })?;
                *item = refreshed.item;
                *item_revision_sha256 = refreshed.revision_sha256;
            }
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "Tier-0 schema-v1 payload for {} could not acquire a stable migration revision after {ACK_REVISION_MAX_ATTEMPTS} attempts",
            item.escalation_id
        ),
    ))
}

async fn remove_tier0_if_terminal(
    db: &Db,
    item: &mut EscalationItem,
    item_revision_sha256: &mut [u8; 32],
) -> Result<bool, ErrorData> {
    if item.status == EscalationStatus::Pending
        || matches!(item.tier0_delivery, Tier0ToastDelivery::Suppressed { .. })
    {
        return Ok(false);
    }
    if matches!(item.tier0_delivery, Tier0ToastDelivery::Removed { .. }) {
        migrate_tier0_payload_v1_if_present(db, item, item_revision_sha256).await?;
        return Ok(false);
    }
    if let Tier0ToastDelivery::RemovalFailed {
        next_retry_at_unix_ms,
        ..
    } = &item.tier0_delivery
        && unix_time_ms_now() < *next_retry_at_unix_ms
    {
        return Ok(false);
    }
    migrate_tier0_payload_v1_if_present(db, item, item_revision_sha256).await?;
    let (tag, expected_group) = tier0_reconciliation_identity(item)?;
    let mut absence_only_outcome = if item.tier0_delivery == Tier0ToastDelivery::NotRequested
        && item.tier0_payload_sha256.is_none()
    {
        let readback = inspect_internal_escalation_toast(tag.clone()).await?;
        if !tier0_readback_identity_shape_valid(item, &tag, &readback) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "terminal pre-Show Tier-0 absence readback has invalid identity/shape: escalation_id={} readback={readback:?}",
                    item.escalation_id
                ),
            ));
        }
        Some(if readback.history_count == 0 {
            ToastRemovalOutcome {
                aumid: SYNAPSE_AUMID.to_owned(),
                tag: tag.clone(),
                group: expected_group.to_owned(),
                status: "not_present".to_owned(),
                removed: false,
                already_absent: true,
                before_count: Some(0),
                after_count: Some(0),
                error_code: None,
                error_message: None,
            }
        } else {
            ToastRemovalOutcome {
                aumid: SYNAPSE_AUMID.to_owned(),
                tag: tag.clone(),
                group: expected_group.to_owned(),
                status: "precondition_failed".to_owned(),
                removed: false,
                already_absent: false,
                before_count: Some(readback.history_count),
                after_count: Some(readback.history_count),
                error_code: Some(error_codes::NOTIFY_DELIVERY_UNVERIFIED.to_owned()),
                error_message: Some(format!(
                    "Tier-0 durable state proves Show was never claimed, but Action Center contains {} reserved Tag+Group row(s); refusing to erase contradictory physical evidence",
                    readback.history_count
                )),
            }
        })
    } else {
        None
    };
    let mut legacy_prepared_payload = None;
    if item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified
        && item.tier0_payload_sha256.is_none()
        && item.tier0_prepared_payload.is_none()
    {
        match prepare_tier0_payload(item).await {
            Ok(prepared) => legacy_prepared_payload = Some(Ok(prepared)),
            Err(error) if legacy_pre_show_contract_rejection(&error) => {
                let readback = inspect_internal_toast(tag.clone()).await?;
                if !tier0_readback_identity_shape_valid(item, &tag, &readback) {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "terminal rejected-legacy Tier-0 readback has invalid identity/shape: escalation_id={} readback={readback:?}",
                            item.escalation_id
                        ),
                    ));
                }
                absence_only_outcome = Some(if readback.history_count == 0 {
                    ToastRemovalOutcome {
                        aumid: SYNAPSE_AUMID.to_owned(),
                        tag: tag.clone(),
                        group: expected_group.to_owned(),
                        status: "not_present".to_owned(),
                        removed: false,
                        already_absent: true,
                        before_count: Some(0),
                        after_count: Some(0),
                        error_code: None,
                        error_message: None,
                    }
                } else {
                    ToastRemovalOutcome {
                        aumid: SYNAPSE_AUMID.to_owned(),
                        tag: tag.clone(),
                        group: expected_group.to_owned(),
                        status: "precondition_failed".to_owned(),
                        removed: false,
                        already_absent: false,
                        before_count: Some(readback.history_count),
                        after_count: Some(readback.history_count),
                        error_code: Some(error_codes::NOTIFY_DELIVERY_UNVERIFIED.to_owned()),
                        error_message: Some(format!(
                            "historical notify validation proves Show was never invoked, but Action Center contains {} legacy Tag+Group row(s); refusing to erase contradictory public-namespace evidence; preparation_error_code={} detail={}",
                            readback.history_count,
                            error_data_symbol(&error),
                            error.message
                        )),
                    }
                });
            }
            Err(error) => legacy_prepared_payload = Some(Err(error)),
        }
    }
    let expected_payload_sha256 = match (
        item.tier0_payload_sha256.clone(),
        item.tier0_prepared_payload.clone(),
    ) {
        (Some(payload_sha256), Some(prepared))
            if is_canonical_sha256(&payload_sha256)
                && prepared_toast_payload_valid(&prepared)
                && tier0_prepared_request_binding_valid(item, &prepared)
                && prepared.payload_sha256 == payload_sha256 =>
        {
            Some(payload_sha256)
        }
        (Some(payload_sha256), prepared) => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 terminal removal has an invalid frozen payload binding: escalation_id={} payload_sha256={payload_sha256:?} prepared_present={}",
                    item.escalation_id,
                    prepared.is_some()
                ),
            ));
        }
        (None, None)
            if item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified
                && absence_only_outcome.is_none() =>
        {
            let prepared_payload = legacy_prepared_payload
                .take()
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "legacy Tier-0 terminal binding lost its preparation verdict: escalation_id={}",
                            item.escalation_id
                        ),
                    )
                })??;
            let payload_sha256 = prepared_payload.payload_sha256.clone();
            let mut bound = false;
            for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
                let current = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "legacy Tier-0 terminal binding lost escalation item {}",
                            item.escalation_id
                        ),
                    )
                })?;
                if current.item.status == EscalationStatus::Pending {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "legacy Tier-0 terminal binding observed pending state: escalation_id={}",
                            item.escalation_id
                        ),
                    ));
                }
                if let Some(durable_prepared) = current.item.tier0_prepared_payload.as_ref() {
                    if durable_prepared != &prepared_payload
                        || current.item.tier0_payload_sha256.as_deref()
                            != Some(payload_sha256.as_str())
                    {
                        return Err(mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "legacy Tier-0 terminal frozen payload differs from its durable binding: escalation_id={} durable_sha256={} actual_sha256={} exact_payload_equal={}",
                                item.escalation_id,
                                durable_prepared.payload_sha256,
                                payload_sha256,
                                durable_prepared == &prepared_payload
                            ),
                        ));
                    }
                    *item = current.item;
                    *item_revision_sha256 = current.revision_sha256;
                    bound = true;
                    break;
                }
                if current.item.tier0_payload_sha256.is_some() {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "legacy Tier-0 terminal row has a digest without its frozen payload: escalation_id={}",
                            item.escalation_id
                        ),
                    ));
                }
                if current.item.tier0_delivery != Tier0ToastDelivery::LegacyUnclassified {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 terminal row lost its payload binding in a non-legacy state: escalation_id={} delivery={:?}",
                            item.escalation_id, current.item.tier0_delivery
                        ),
                    ));
                }
                let mut updated = current.item;
                updated.tier0_payload_sha256 = Some(payload_sha256.clone());
                updated.tier0_prepared_payload = Some(prepared_payload.clone());
                updated.updated_at_unix_ms = unix_time_ms_now().max(updated.updated_at_unix_ms);
                match write_item_and_audit_if_revision(
                    db,
                    &updated,
                    "tier0_legacy_payload_bound",
                    json!({
                        "tag": tag,
                        "group": expected_group,
                        "payload_sha256": payload_sha256,
                        "payload_schema_version": prepared_payload.schema_version,
                        "purpose": "terminal physical removal",
                    }),
                    current.revision_sha256,
                )? {
                    ItemWriteOutcome::Applied { committed_seq, .. } => {
                        let readback = read_item_revisioned(db, &updated.escalation_id)?
                            .ok_or_else(|| {
                                mcp_error(
                                    error_codes::STORAGE_READ_FAILED,
                                    format!(
                                        "legacy Tier-0 terminal binding disappeared after committed_seq={committed_seq}: escalation_id={}",
                                        updated.escalation_id
                                    ),
                                )
                            })?;
                        if readback.item.tier0_payload_sha256 != Some(payload_sha256.clone())
                            || readback.item.tier0_prepared_payload.as_ref()
                                != Some(&prepared_payload)
                        {
                            return Err(mcp_error(
                                error_codes::STORAGE_CORRUPTED,
                                format!(
                                    "legacy Tier-0 terminal binding readback differed: escalation_id={} expected_sha256={} actual={:?} exact_payload_equal={} committed_seq={committed_seq}",
                                    updated.escalation_id,
                                    payload_sha256,
                                    readback.item.tier0_payload_sha256,
                                    readback.item.tier0_prepared_payload.as_ref()
                                        == Some(&prepared_payload)
                                ),
                            ));
                        }
                        *item = readback.item;
                        *item_revision_sha256 = readback.revision_sha256;
                        bound = true;
                        break;
                    }
                    ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                        code = "ESCALATION_TIER0_LEGACY_TERMINAL_BIND_REVISION_RETRY",
                        escalation_id = %item.escalation_id,
                        revision_attempt,
                        observed_seq,
                        "legacy Tier-0 terminal binding raced another item update; rereading"
                    ),
                }
            }
            if !bound {
                return Err(mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "legacy Tier-0 terminal payload binding could not acquire a stable revision after {ACK_REVISION_MAX_ATTEMPTS} attempts: escalation_id={}",
                        item.escalation_id
                    ),
                ));
            }
            Some(payload_sha256)
        }
        (None, None) if absence_only_outcome.is_some() => None,
        (None, prepared) => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 terminal removal lacks a complete durable payload binding: escalation_id={} delivery={:?} prepared_present={}",
                    item.escalation_id,
                    item.tier0_delivery,
                    prepared.is_some()
                ),
            ));
        }
    };
    let outcome = if let Some(outcome) = absence_only_outcome {
        outcome
    } else {
        let expected_payload_sha256 = expected_payload_sha256.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 removal lost its exact payload binding: escalation_id={}",
                    item.escalation_id
                ),
            )
        })?;
        match expected_group {
            SYNAPSE_ESCALATION_TOAST_GROUP => {
                remove_internal_escalation_toast(
                    tag.clone(),
                    expected_payload_sha256,
                    item.expires_at_unix_ms,
                    item.created_at_unix_ms,
                )
                .await
            }
            SYNAPSE_TOAST_GROUP => {
                remove_internal_toast(tag.clone(), expected_payload_sha256).await
            }
            _ => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Tier-0 removal selected unsupported group {expected_group:?} for escalation {}",
                        item.escalation_id
                    ),
                ));
            }
        }
    };
    if outcome.tag != tag
        || outcome.aumid != SYNAPSE_AUMID
        || outcome.group != expected_group
        || outcome.removed && outcome.already_absent
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 removal returned an invalid identity/disposition: escalation_id={} expected_tag={} outcome={outcome:?}",
                item.escalation_id, tag
            ),
        ));
    }
    let physically_absent = (outcome.removed || outcome.already_absent)
        && outcome.after_count == Some(0)
        && outcome.error_code.is_none()
        && outcome.error_message.is_none();
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let current = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("Tier-0 removal lost escalation item {}", item.escalation_id),
            )
        })?;
        if let Tier0ToastDelivery::RemovalFailed {
            next_retry_at_unix_ms,
            ..
        } = &current.item.tier0_delivery
            && unix_time_ms_now() < *next_retry_at_unix_ms
        {
            // Another sweep already durably scheduled the next physical retry.
            *item = current.item;
            *item_revision_sha256 = current.revision_sha256;
            return Ok(false);
        }
        if current.item.status == EscalationStatus::Pending {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 toast was physically removed while escalation {} became pending",
                    item.escalation_id
                ),
            ));
        }
        if matches!(
            current.item.tier0_delivery,
            Tier0ToastDelivery::Removed { .. }
        ) {
            *item = current.item;
            *item_revision_sha256 = current.revision_sha256;
            return Ok(false);
        }
        let recorded_at = unix_time_ms_now().max(current.item.updated_at_unix_ms);
        let (removal_attempt_count, removal_failed_at, unchanged_failure) =
            match &current.item.tier0_delivery {
                Tier0ToastDelivery::RemovalFailed {
                    removal,
                    attempt_count,
                    failed_at_unix_ms,
                    ..
                } => (
                    attempt_count.saturating_add(1),
                    *failed_at_unix_ms,
                    removal == &outcome
                        && current.item.tier0_toast_removed.as_ref() == Some(&outcome),
                ),
                _ => (1, recorded_at, false),
            };
        // An intent that was stuck in `removal_failed` (e.g. the #1803
        // ordinary-TTL toasts whose Action Center expiration was capped to
        // `arrival + 3 days`, which the old exact-equality precondition refused
        // to remove) converging to physically absent is a one-time contract
        // repair worth attributing.
        let recovered_from_removal_failed = matches!(
            current.item.tier0_delivery,
            Tier0ToastDelivery::RemovalFailed { .. }
        );
        let prior_removal_attempts = removal_attempt_count.saturating_sub(1);
        let mut updated = current.item;
        let delivery_proof_promoted = outcome.removed && outcome.before_count == Some(1);
        if delivery_proof_promoted {
            // The removal API only reports `removed` after a separate
            // pre-mutation history read proved exactly one row with the bound
            // payload (and reserved-group expiration, when applicable). That
            // physical proof is stronger than the legacy compatibility bit and
            // must survive the terminal removal transition.
            updated.tier0_fired = true;
        }
        updated.tier0_toast_removed = Some(outcome.clone());
        updated.tier0_delivery = if physically_absent {
            Tier0ToastDelivery::Removed {
                tag: tag.clone(),
                removal: outcome.clone(),
                removed_at_unix_ms: recorded_at,
            }
        } else {
            Tier0ToastDelivery::RemovalFailed {
                tag: tag.clone(),
                removal: outcome.clone(),
                attempt_count: removal_attempt_count,
                last_checked_at_unix_ms: recorded_at,
                next_retry_at_unix_ms: recorded_at
                    .saturating_add(tier0_removal_retry_delay_ms(removal_attempt_count)),
                failed_at_unix_ms: removal_failed_at,
            }
        };
        updated.updated_at_unix_ms = recorded_at;
        let event = if physically_absent {
            "tier0_toast_removed"
        } else if unchanged_failure {
            "tier0_toast_removal_retry_unchanged"
        } else {
            "tier0_toast_removal_failed"
        };
        match write_item_and_audit_if_revision(
            db,
            &updated,
            event,
            json!({
                "toast_removal": &outcome,
                "physically_absent": physically_absent,
                "delivery_proof_promoted": delivery_proof_promoted,
                "delivery_proof_source": delivery_proof_promoted.then_some(
                    "exact pre-removal Action Center payload and expiration readback"
                ),
                "unchanged_failure": unchanged_failure,
                "attempt_count": (!physically_absent).then_some(removal_attempt_count),
                "next_retry_at_unix_ms": (!physically_absent).then_some(
                    recorded_at.saturating_add(
                        tier0_removal_retry_delay_ms(removal_attempt_count)
                    )
                ),
                "source_of_truth": "Windows Action Center history",
            }),
            current.revision_sha256,
        )? {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback = read_item_revisioned(db, &item.escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "Tier-0 removal item {} disappeared after committed_seq={committed_seq}",
                            item.escalation_id
                        ),
                    )
                })?;
                if readback.item.tier0_delivery != updated.tier0_delivery
                    || readback.item.tier0_toast_removed.as_ref() != Some(&outcome)
                    || readback.item.tier0_fired != updated.tier0_fired
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 removal readback differed: escalation_id={} expected={:?} actual={:?} committed_seq={committed_seq}",
                            item.escalation_id,
                            updated.tier0_delivery,
                            readback.item.tier0_delivery
                        ),
                    ));
                }
                *item = readback.item;
                *item_revision_sha256 = readback.revision_sha256;
                if physically_absent && recovered_from_removal_failed {
                    tracing::info!(
                        code = "ESCALATION_TIER0_EXPIRATION_CONTRACT_REPAIRED",
                        escalation_id = %item.escalation_id,
                        prior_removal_attempts,
                        expires_at_unix_ms = item.expires_at_unix_ms,
                        "readback=CF_KV Action Center a Tier-0 intent previously stuck in removal_failed converged to Removed after the Windows retention-cap expiration contract was reconciled (#1762/#1803); the 60s removal retry loop is terminated"
                    );
                }
                tracing::info!(
                    code = "ESCALATION_TIER0_TOAST_REMOVAL_DURABLE",
                    escalation_id = %item.escalation_id,
                    status = %outcome.status,
                    physically_absent,
                    removed = outcome.removed,
                    already_absent = outcome.already_absent,
                    before_count = outcome.before_count,
                    after_count = outcome.after_count,
                    tier0_fired = item.tier0_fired,
                    delivery_proof_promoted,
                    revision_attempt,
                    committed_seq,
                    "readback=CF_KV Action Center Tag+Group removal result stored"
                );
                return Ok(physically_absent);
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                code = "ESCALATION_TIER0_REMOVAL_REVISION_RETRY",
                escalation_id = %item.escalation_id,
                revision_attempt,
                observed_seq,
                "Tier-0 removal result raced an item mutation; rereading without repeating WinRT removal"
            ),
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "Tier-0 removal result for {} could not acquire a stable item revision after {ACK_REVISION_MAX_ATTEMPTS} attempts",
            item.escalation_id
        ),
    ))
}

fn tier0_removal_failed(item: &EscalationItem) -> bool {
    matches!(
        item.tier0_delivery,
        Tier0ToastDelivery::RemovalFailed { .. }
    )
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

fn applied_projection_for_tier0_claim(
    db: &Db,
    item: &EscalationItem,
    now_unix_ms: u64,
) -> Result<Option<RevisionedTransitionProjectionWatermark>, ErrorData> {
    let authoritative_reads = super::agent_state::reads(now_unix_ms);
    let Some(authoritative_agent) = authoritative_agent_read_for_item(&authoritative_reads, item)
    else {
        return Ok(None);
    };
    if authoritative_agent.state == AgentLifecycleState::Dead
        || authoritative_agent.state.as_str() != item.attention_state
    {
        return Ok(None);
    }
    let projection = read_projection_watermark(db, &item.anchor)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 toast claim has no transition projection cursor: escalation_id={} anchor={:?}",
                item.escalation_id, item.anchor
            ),
        )
    })?;
    if projection.record.phase != TransitionProjectionPhase::Applied
        || projection.record.observed.anchor != item.anchor
        || projection.record.observed.state_to != item.attention_state
    {
        return Ok(None);
    }
    verify_applied_projection_evidence(db, &projection.record)?;
    let application = projection
        .record
        .applied_evidence
        .as_ref()
        .map(|evidence| &evidence.application)
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Applied transition projection has no evidence at Tier-0 claim: escalation_id={} anchor={:?}",
                    item.escalation_id, item.anchor
                ),
            )
        })?;
    if !matches!(
        application,
        TransitionProjectionApplication::Escalation {
            escalation_id,
            approval_id,
        } if escalation_id == &item.escalation_id && approval_id == &item.approval_id
    ) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Applied projection is not bound to the escalation/approval claimed for Tier-0 delivery: escalation_id={} approval_id={}",
                item.escalation_id, item.approval_id
            ),
        ));
    }
    Ok(Some(projection))
}

enum Tier0ToastClaim {
    Started(EscalationItem, Tier0LiveClaim),
    NeedsHistory(EscalationItem),
    OwnedByLiveCaller,
    NoAction,
}

struct Tier0ClaimExecution {
    claim: Tier0ToastClaim,
    send_result: Option<Result<NotifyHumanResponse, ErrorData>>,
    expired_before_show: bool,
}

#[derive(Default)]
struct Tier0AuthorizationState {
    claim: Option<Tier0ToastClaim>,
    expired_before_show: bool,
}

fn tier0_pre_show_failure(error: ErrorData, escalation_id: &str) -> ToastPreShowFailure {
    let original_code = error_data_symbol(&error);
    ToastPreShowFailure {
        code: error_codes::STORAGE_WRITE_FAILED,
        message: format!(
            "Tier-0 COM queue-head authorization failed: escalation_id={escalation_id} original_code={original_code} detail={}",
            error.message
        ),
    }
}

async fn claim_and_fire_tier0(
    db: Arc<Db>,
    notify_item: EscalationItem,
    prepared_payload: PreparedToastPayload,
) -> Result<Tier0ClaimExecution, ErrorData> {
    let escalation_id = notify_item.escalation_id.clone();
    let collision_prepared_payload = prepared_payload.clone();
    let expected_preflight = prepared_payload.clone();
    let authorization_state = Arc::new(Mutex::new(Tier0AuthorizationState::default()));
    let authorization_state_for_worker = Arc::clone(&authorization_state);
    let authorization_db = Arc::clone(&db);
    let authorization_escalation_id = escalation_id.clone();
    let pre_show_authorizer: ToastPreShowAuthorizer = Box::new(move |prepared| {
        if prepared != &expected_preflight {
            return Err(ToastPreShowFailure {
                code: error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                message: format!(
                    "Tier-0 frozen payload changed between durable selection and the COM queue head: escalation_id={authorization_escalation_id} expected_sha256={} actual_sha256={} exact_payload_equal=false",
                    expected_preflight.payload_sha256, prepared.payload_sha256
                ),
            });
        }
        let transition_guard = super::agent_state::acquire_transition_pipeline_lock().map_err(
            |detail| ToastPreShowFailure {
                code: error_codes::STORAGE_WRITE_FAILED,
                message: format!(
                    "Tier-0 COM queue-head authorization could not acquire the transition boundary: escalation_id={authorization_escalation_id} detail={detail}"
                ),
            },
        )?;
        let claim = claim_tier0_toast_locked(
            &authorization_db,
            &authorization_escalation_id,
            Some(prepared),
        )
        .map_err(|error| tier0_pre_show_failure(error, &authorization_escalation_id))?;
        let mut state = match authorization_state_for_worker.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                if matches!(&claim, Tier0ToastClaim::Started(..)) {
                    restore_known_unsent_before_show_locked(
                        &authorization_db,
                        &authorization_escalation_id,
                        "authorization_result_state_poisoned",
                    )
                    .map_err(|error| {
                        let mut failure =
                            tier0_pre_show_failure(error, &authorization_escalation_id);
                        failure.message =
                            format!("{}; authorization_state_poison={poisoned}", failure.message);
                        failure
                    })?;
                }
                return Err(ToastPreShowFailure {
                    code: error_codes::STORAGE_WRITE_FAILED,
                    message: format!(
                        "Tier-0 authorization result state is poisoned: escalation_id={authorization_escalation_id} detail={poisoned}"
                    ),
                });
            }
        };
        match claim {
            Tier0ToastClaim::Started(item, live_claim) => {
                state.claim = Some(Tier0ToastClaim::Started(item, live_claim));
                drop(state);
                let expiry_db = Arc::clone(&authorization_db);
                let expiry_escalation_id = authorization_escalation_id.clone();
                let expiry_state = Arc::clone(&authorization_state_for_worker);
                Ok(Some(ToastShowAuthority::new(
                    transition_guard,
                    move |reason| {
                        restore_known_unsent_before_show_locked(
                            &expiry_db,
                            &expiry_escalation_id,
                            reason,
                        )
                        .map_err(|error| tier0_pre_show_failure(error, &expiry_escalation_id))?;
                        let mut state =
                            expiry_state
                                .lock()
                                .map_err(|poisoned| ToastPreShowFailure {
                                    code: error_codes::STORAGE_WRITE_FAILED,
                                    message: format!(
                                        "Tier-0 expiry result state is poisoned: escalation_id={expiry_escalation_id} detail={poisoned}"
                                    ),
                                })?;
                        state.claim = Some(Tier0ToastClaim::NoAction);
                        state.expired_before_show = reason == "deadline_expired_before_show";
                        Ok(())
                    },
                )))
            }
            claim => {
                state.claim = Some(claim);
                drop(state);
                drop(transition_guard);
                Ok(None)
            }
        }
    });

    let send_result = tokio::task::spawn_blocking(move || {
        fire_tier0_blocking(&notify_item, prepared_payload, pre_show_authorizer)
    })
    .await
    .map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("Tier-0 blocking notify-queue task failed to join: {error}"),
        )
    })?;

    let (claim, expired_before_show) = {
        let mut state = authorization_state.lock().map_err(|poisoned| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "Tier-0 authorization result state is poisoned after notify completion: escalation_id={escalation_id} detail={poisoned}"
                ),
            )
        })?;
        (state.claim.take(), state.expired_before_show)
    };
    let Some(claim) = claim else {
        return match send_result {
            Err(error) => {
                let tag = escalation_toast_tag(&escalation_id);
                let physical_readback = inspect_internal_escalation_toast(tag.clone())
                    .await
                    .map_err(|inspection_error| {
                        mcp_error(
                            error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                            format!(
                                "Tier-0 toast failed before the queue-head authorizer and the mandatory collision readback also failed: escalation_id={escalation_id} original_error={}; inspection_error_code={} inspection_error={}",
                                error.message,
                                error_data_symbol(&inspection_error),
                                inspection_error.message
                            ),
                        )
                    })?;
                if physical_readback.present {
                    let classified = classify_tier0_pre_show_collision(
                        &db,
                        &escalation_id,
                        &collision_prepared_payload,
                        &physical_readback,
                        &error,
                    )?;
                    tracing::warn!(
                        code = "ESCALATION_TIER0_PRE_SHOW_COLLISION_OBSERVED",
                        escalation_id,
                        tag,
                        classified,
                        history_count = physical_readback.history_count,
                        payload_sha256s = ?physical_readback.payload_sha256s,
                        expiration_unix_ms = ?physical_readback.expiration_unix_ms,
                        original_error_code = error_data_symbol(&error),
                        "readback=Action Center contained reserved Tag+Group rows before this caller reached the ToastNotifier.Show authorizer"
                    );
                } else {
                    tracing::warn!(
                        code = "ESCALATION_TIER0_PRE_SHOW_FAILURE_ABSENT",
                        escalation_id,
                        tag,
                        original_error_code = error_data_symbol(&error),
                        "readback=Action Center exact reserved Tag+Group was absent after failure before the ToastNotifier.Show authorizer"
                    );
                }
                Err(error)
            }
            Ok(_) => Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 notify job completed without a queue-head authorization verdict: escalation_id={escalation_id}"
                ),
            )),
        };
    };
    let send_result = match send_result {
        Err(error) if matches!(claim, Tier0ToastClaim::NoAction) && !expired_before_show => {
            return Err(error);
        }
        Ok(Some(response)) => Some(Ok(response)),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    };
    Ok(Tier0ClaimExecution {
        claim,
        send_result,
        expired_before_show,
    })
}

fn tier0_projection_guard(
    item: &EscalationItem,
    projection: &RevisionedTransitionProjectionWatermark,
) -> GuardedExtraRows {
    GuardedExtraRows {
        rows: Vec::new(),
        guards: vec![RevisionGuard::new(
            projection_watermark_key(&item.anchor),
            Some(projection.revision_sha256),
        )],
    }
}

fn classify_tier0_pre_show_collision(
    db: &Db,
    escalation_id: &str,
    prepared_payload: &PreparedToastPayload,
    history_readback: &ToastHistoryReadback,
    send_error: &ErrorData,
) -> Result<bool, ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| {
        classify_tier0_pre_show_collision_locked(
            db,
            escalation_id,
            prepared_payload,
            history_readback,
            send_error,
        )
    })
    .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "Tier-0 pre-Show collision classification could not acquire the transition/ack boundary: escalation_id={escalation_id} detail={detail}"
            ),
        )
    })?
}

fn classify_tier0_pre_show_collision_locked(
    db: &Db,
    escalation_id: &str,
    prepared_payload: &PreparedToastPayload,
    history_readback: &ToastHistoryReadback,
    send_error: &ErrorData,
) -> Result<bool, ErrorData> {
    if !history_readback.present || history_readback.history_count == 0 {
        return Ok(false);
    }
    if !prepared_toast_payload_valid(prepared_payload) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 pre-Show collision classification received an invalid frozen payload: escalation_id={escalation_id} payload_sha256={:?}",
                prepared_payload.payload_sha256
            ),
        ));
    }
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let current = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 pre-Show collision classification lost escalation item {escalation_id}"
                ),
            )
        })?;
        if current.item.status != EscalationStatus::Pending
            || current.item.tier0_suppressed_reason.is_some()
        {
            return Ok(false);
        }
        if !tier0_prepared_request_binding_valid(&current.item, prepared_payload) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 pre-Show collision frozen payload is not bound to the current escalation request: escalation_id={escalation_id} payload_sha256={} intent_sha256={}",
                    prepared_payload.payload_sha256, prepared_payload.intent_sha256
                ),
            ));
        }
        match &current.item.tier0_delivery {
            Tier0ToastDelivery::NotRequested => {}
            Tier0ToastDelivery::KnownUnsent { .. } => {
                if current.item.tier0_payload_sha256.as_deref()
                    != Some(prepared_payload.payload_sha256.as_str())
                    || current.item.tier0_prepared_payload.as_ref() != Some(prepared_payload)
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 pre-Show collision payload disagrees with durable KnownUnsent intent: escalation_id={escalation_id} durable_sha256={:?} observed_sha256={} exact_payload_equal={}",
                            current.item.tier0_payload_sha256,
                            prepared_payload.payload_sha256,
                            current.item.tier0_prepared_payload.as_ref() == Some(prepared_payload)
                        ),
                    ));
                }
            }
            Tier0ToastDelivery::PreShowCollision { .. } => return Ok(true),
            Tier0ToastDelivery::LegacyUnclassified
            | Tier0ToastDelivery::StartedUnknown { .. }
            | Tier0ToastDelivery::VerifiedPresent { .. }
            | Tier0ToastDelivery::VerifiedDismissed { .. }
            | Tier0ToastDelivery::Failed { .. }
            | Tier0ToastDelivery::RemovalFailed { .. }
            | Tier0ToastDelivery::Removed { .. }
            | Tier0ToastDelivery::Suppressed { .. } => return Ok(false),
        }
        let classify_now_unix_ms =
            checked_unix_time_ms("Tier-0 pre-Show physical collision classification boundary")?
                .max(current.item.updated_at_unix_ms);
        let Some(projection) =
            applied_projection_for_tier0_claim(db, &current.item, classify_now_unix_ms)?
        else {
            return Ok(false);
        };
        let expected_tag = escalation_toast_tag(escalation_id);
        if history_readback.tag != expected_tag
            || !tier0_readback_identity_shape_valid(&current.item, &expected_tag, history_readback)
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 pre-Show collision readback has an invalid reserved identity/shape: escalation_id={escalation_id} expected_tag={expected_tag} readback={history_readback:?}"
                ),
            ));
        }
        let generation = projection.record.observed.generation;
        let mut item = current.item;
        item.tier0_payload_sha256 = Some(prepared_payload.payload_sha256.clone());
        item.tier0_prepared_payload = Some(prepared_payload.clone());
        let expected_expiration_unix_ms = Some(item.expires_at_unix_ms);
        // A prior reserved row carries the Windows-capped expiration
        // (`arrival + 3 days`), not the durable deadline, for ordinary-TTL
        // escalations (#1803). Recognize that capped value as the exact
        // pre-existing row so a legitimate prior delivery is deferred to the
        // durable dedupe path rather than misquarantined as a collision.
        let stored_expiration_unix_ms = history_readback
            .expiration_unix_ms
            .first()
            .copied()
            .flatten();
        let expiration_contract_matches = match stored_expiration_unix_ms {
            Some(stored) => platform_corrected_expiration(
                item.expires_at_unix_ms,
                stored,
                item.created_at_unix_ms,
                classify_now_unix_ms,
            )
            .is_some(),
            None => false,
        };
        let physical_contract_matches = history_readback.history_count == 1
            && history_readback.payload_sha256s.first().map(String::as_str)
                == Some(prepared_payload.payload_sha256.as_str())
            && expiration_contract_matches;
        if physical_contract_matches {
            tracing::info!(
                code = "ESCALATION_TIER0_PRE_SHOW_EXACT_ROW_DEFERRED",
                escalation_id,
                revision_attempt,
                "Action Center contains one exact pre-existing row; leaving the item retriable so the normal durable claim/dedupe path can attribute physical delivery"
            );
            return Ok(false);
        }
        let error_code = error_data_symbol(send_error);
        let error_message = format!(
            "{}; Action Center already contained the reserved Tag+Group before this caller reached the ToastNotifier.Show authorizer; expected_sha256={} actual_sha256={:?} expected_expiration_unix_ms={expected_expiration_unix_ms:?} actual_expiration_unix_ms={:?} history_count={} physical_contract_matches={physical_contract_matches}; Show was not invoked and the physical rows are quarantined",
            send_error.message,
            prepared_payload.payload_sha256,
            history_readback.payload_sha256s,
            history_readback.expiration_unix_ms,
            history_readback.history_count,
        );
        item.tier0_fired = false;
        item.tier0_delivery = Tier0ToastDelivery::PreShowCollision {
            tag: expected_tag.clone(),
            projection_generation: generation,
            error_code: error_code.clone(),
            error_message: error_message.clone(),
            history_readback: history_readback.clone(),
            classified_at_unix_ms: classify_now_unix_ms,
        };
        item.updated_at_unix_ms = classify_now_unix_ms;
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            "tier0_pre_show_physical_collision",
            json!({
                "tag": history_readback.tag,
                "group": history_readback.group,
                "history_readback": history_readback,
                "expected_payload_sha256": prepared_payload.payload_sha256,
                "expected_expiration_unix_ms": expected_expiration_unix_ms,
                "physical_contract_matches": physical_contract_matches,
                "toast_show_invoked": false,
                "error_code": error_code,
                "error_message": error_message,
                "source_of_truth": "Windows Action Center history",
            }),
            tier0_projection_guard(&item, &projection),
            current.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let actual = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "Tier-0 pre-Show collision item {escalation_id} disappeared after committed_seq={committed_seq}"
                        ),
                    )
                })?;
                if actual.item.tier0_delivery != item.tier0_delivery
                    || actual.item.tier0_payload_sha256 != item.tier0_payload_sha256
                    || actual.item.tier0_prepared_payload != item.tier0_prepared_payload
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 pre-Show collision durable readback differed: escalation_id={escalation_id} expected={:?} actual={:?} committed_seq={committed_seq}",
                            item.tier0_delivery, actual.item.tier0_delivery
                        ),
                    ));
                }
                tracing::warn!(
                    code = "ESCALATION_TIER0_PRE_SHOW_COLLISION_DURABLE",
                    escalation_id,
                    revision_attempt,
                    committed_seq,
                    history_count = history_readback.history_count,
                    payload_sha256s = ?history_readback.payload_sha256s,
                    expiration_unix_ms = ?history_readback.expiration_unix_ms,
                    "readback=CF_KV preserves full Action Center collision evidence; ToastNotifier.Show was not invoked"
                );
                return Ok(true);
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                code = "ESCALATION_TIER0_PRE_SHOW_COLLISION_REVISION_RETRY",
                escalation_id,
                revision_attempt,
                observed_seq,
                "Tier-0 pre-Show collision classification raced another item/projection update; rereading"
            ),
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "Tier-0 pre-Show collision for {escalation_id} could not acquire stable item/projection revisions after {ACK_REVISION_MAX_ATTEMPTS} attempts"
        ),
    ))
}

fn claim_tier0_toast_locked(
    db: &Db,
    escalation_id: &str,
    prepared_payload: Option<&PreparedToastPayload>,
) -> Result<Tier0ToastClaim, ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let claim_now_unix_ms = checked_unix_time_ms("Tier-0 durable claim boundary")?;
        let current = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("Tier-0 toast claim has no escalation item {escalation_id}"),
            )
        })?;
        if current.item.status != EscalationStatus::Pending
            || current.item.tier0_suppressed_reason.is_some()
            || claim_now_unix_ms >= current.item.expires_at_unix_ms
        {
            return Ok(Tier0ToastClaim::NoAction);
        }
        match &current.item.tier0_delivery {
            Tier0ToastDelivery::LegacyUnclassified => {
                let prepared_payload = prepared_payload.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "legacy Tier-0 reconciliation has no canonical WinRT payload preflight: escalation_id={escalation_id}"
                        ),
                    )
                })?;
                if !prepared_toast_payload_valid(prepared_payload)
                    || !tier0_prepared_request_binding_valid(&current.item, prepared_payload)
                    || prepared_payload.suppress_popup != current.item.tier0_quiet_digest
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "legacy Tier-0 reconciliation received an invalid frozen payload: escalation_id={escalation_id} payload_sha256={:?}",
                            prepared_payload.payload_sha256
                        ),
                    ));
                }
                if let Some(durable_prepared) = current.item.tier0_prepared_payload.as_ref() {
                    if durable_prepared != prepared_payload
                        || current.item.tier0_payload_sha256.as_deref()
                            != Some(prepared_payload.payload_sha256.as_str())
                    {
                        return Err(mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "legacy Tier-0 frozen payload changed after durable binding: escalation_id={escalation_id} durable_sha256={} actual_sha256={} exact_payload_equal={}",
                                durable_prepared.payload_sha256,
                                prepared_payload.payload_sha256,
                                durable_prepared == prepared_payload
                            ),
                        ));
                    }
                    return Ok(Tier0ToastClaim::NeedsHistory(current.item));
                }
                if current.item.tier0_payload_sha256.is_some() {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "legacy Tier-0 row has a digest without its frozen payload: escalation_id={escalation_id}"
                        ),
                    ));
                }
                let mut bound = current.item;
                bound.tier0_payload_sha256 = Some(prepared_payload.payload_sha256.clone());
                bound.tier0_prepared_payload = Some(prepared_payload.clone());
                bound.updated_at_unix_ms = claim_now_unix_ms.max(bound.updated_at_unix_ms);
                match write_item_and_audit_if_revision(
                    db,
                    &bound,
                    "tier0_legacy_payload_bound",
                    json!({
                        "tag": legacy_escalation_toast_tag(escalation_id),
                        "group": SYNAPSE_TOAST_GROUP,
                        "payload_sha256": prepared_payload.payload_sha256,
                        "payload_schema_version": prepared_payload.schema_version,
                        "source": "frozen legacy template canonicalized by WinRT LoadXml/GetXml",
                    }),
                    current.revision_sha256,
                )? {
                    ItemWriteOutcome::Applied { committed_seq, .. } => {
                        let readback = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
                            mcp_error(
                                error_codes::STORAGE_READ_FAILED,
                                format!(
                                    "legacy Tier-0 item {escalation_id} disappeared after payload binding committed_seq={committed_seq}"
                                ),
                            )
                        })?;
                        if readback.item.tier0_payload_sha256 != bound.tier0_payload_sha256
                            || readback.item.tier0_prepared_payload != bound.tier0_prepared_payload
                        {
                            return Err(mcp_error(
                                error_codes::STORAGE_CORRUPTED,
                                format!(
                                    "legacy Tier-0 frozen payload binding readback differed: escalation_id={escalation_id} expected_sha256={:?} actual_sha256={:?} exact_payload_equal={} committed_seq={committed_seq}",
                                    bound.tier0_payload_sha256,
                                    readback.item.tier0_payload_sha256,
                                    readback.item.tier0_prepared_payload
                                        == bound.tier0_prepared_payload
                                ),
                            ));
                        }
                        return Ok(Tier0ToastClaim::NeedsHistory(readback.item));
                    }
                    ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                        code = "ESCALATION_TIER0_LEGACY_BIND_REVISION_RETRY",
                        escalation_id,
                        revision_attempt,
                        observed_seq,
                        "legacy Tier-0 payload binding raced an item transition; rereading"
                    ),
                }
                continue;
            }
            Tier0ToastDelivery::StartedUnknown { owner_epoch, .. } => {
                if owner_epoch == daemon_epoch() && Tier0LiveClaim::is_registered(escalation_id)? {
                    return Ok(Tier0ToastClaim::OwnedByLiveCaller);
                }
                return Ok(Tier0ToastClaim::NeedsHistory(current.item));
            }
            Tier0ToastDelivery::VerifiedPresent { .. }
            | Tier0ToastDelivery::VerifiedDismissed { .. }
            | Tier0ToastDelivery::PreShowCollision { .. }
            | Tier0ToastDelivery::Failed { .. }
            | Tier0ToastDelivery::RemovalFailed { .. }
            | Tier0ToastDelivery::Removed { .. }
            | Tier0ToastDelivery::Suppressed { .. } => {
                return Ok(Tier0ToastClaim::NoAction);
            }
            Tier0ToastDelivery::NotRequested | Tier0ToastDelivery::KnownUnsent { .. } => {}
        }
        let Some(projection) =
            applied_projection_for_tier0_claim(db, &current.item, claim_now_unix_ms)?
        else {
            return Ok(Tier0ToastClaim::NoAction);
        };
        let generation = projection.record.observed.generation;
        let tag = escalation_toast_tag(escalation_id);
        let mut item = current.item;
        let mut live_claim_for_started = None;
        let (event, detail) = match &item.tier0_delivery {
            Tier0ToastDelivery::NotRequested => {
                let prepared_payload = prepared_payload.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 claim reached NotRequested without a successful WinRT payload preflight: escalation_id={escalation_id}"
                        ),
                    )
                })?;
                if !prepared_toast_payload_valid(prepared_payload)
                    || !tier0_prepared_request_binding_valid(&item, prepared_payload)
                    || prepared_payload.suppress_popup != item.tier0_quiet_digest
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 claim received an invalid frozen preflight payload: escalation_id={escalation_id} payload_sha256={:?}",
                            prepared_payload.payload_sha256
                        ),
                    ));
                }
                item.tier0_payload_sha256 = Some(prepared_payload.payload_sha256.clone());
                item.tier0_prepared_payload = Some(prepared_payload.clone());
                item.tier0_delivery = Tier0ToastDelivery::KnownUnsent {
                    tag: tag.clone(),
                    projection_generation: generation,
                    prepared_at_unix_ms: claim_now_unix_ms.max(item.updated_at_unix_ms),
                };
                (
                    "tier0_intent_prepared",
                    json!({
                        "tag": tag,
                        "state": "known_unsent",
                        "projection_generation": generation,
                        "toast_show_not_started": true,
                        "payload_sha256": prepared_payload.payload_sha256,
                        "payload_schema_version": prepared_payload.schema_version,
                    }),
                )
            }
            Tier0ToastDelivery::KnownUnsent {
                projection_generation,
                ..
            } => {
                let prepared_payload = prepared_payload.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 claim reached KnownUnsent without its durable frozen payload: escalation_id={escalation_id}"
                        ),
                    )
                })?;
                if item.tier0_payload_sha256.as_deref()
                    != Some(prepared_payload.payload_sha256.as_str())
                    || item.tier0_prepared_payload.as_ref() != Some(prepared_payload)
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 frozen payload changed after durable preparation: escalation_id={escalation_id} durable_sha256={:?} selected_sha256={} exact_payload_equal={}",
                            item.tier0_payload_sha256,
                            prepared_payload.payload_sha256,
                            item.tier0_prepared_payload.as_ref() == Some(prepared_payload)
                        ),
                    ));
                }
                live_claim_for_started = Some(Tier0LiveClaim::register(escalation_id)?);
                let previous_generation = *projection_generation;
                item.tier0_delivery = Tier0ToastDelivery::StartedUnknown {
                    tag: tag.clone(),
                    projection_generation: generation,
                    owner_epoch: daemon_epoch().to_owned(),
                    started_at_unix_ms: claim_now_unix_ms.max(item.updated_at_unix_ms),
                };
                (
                    "tier0_started",
                    json!({
                        "tag": tag,
                        "state": "started_unknown",
                        "previous_projection_generation": previous_generation,
                        "projection_generation": generation,
                        "remediation": "a crash after this boundary requires Action Center Tag+Group reconciliation and never blind replay",
                    }),
                )
            }
            _ => unreachable!("Tier-0 claim states were filtered above"),
        };
        if matches!(
            item.tier0_delivery,
            Tier0ToastDelivery::StartedUnknown { .. }
        ) && live_claim_for_started.is_none()
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 StartedUnknown transition has no pre-commit live-claim reservation: escalation_id={escalation_id}"
                ),
            ));
        }
        item.updated_at_unix_ms = claim_now_unix_ms.max(item.updated_at_unix_ms);
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            event,
            detail,
            tier0_projection_guard(&item, &projection),
            current.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "Tier-0 claim item {escalation_id} disappeared after committed_seq={committed_seq}"
                        ),
                    )
                })?;
                if readback.item.tier0_delivery != item.tier0_delivery
                    || readback.item.tier0_payload_sha256 != item.tier0_payload_sha256
                    || readback.item.tier0_prepared_payload != item.tier0_prepared_payload
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 claim readback state differed: escalation_id={escalation_id} expected_delivery={:?} actual_delivery={:?} expected_sha256={:?} actual_sha256={:?} exact_payload_equal={} committed_seq={committed_seq}",
                            item.tier0_delivery,
                            readback.item.tier0_delivery,
                            item.tier0_payload_sha256,
                            readback.item.tier0_payload_sha256,
                            readback.item.tier0_prepared_payload == item.tier0_prepared_payload
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_TIER0_CLAIM_DURABLE",
                    escalation_id,
                    event,
                    revision_attempt,
                    committed_seq,
                    state = ?item.tier0_delivery,
                    "readback=CF_KV Tier-0 state and exact Applied projection guard committed before ToastNotifier.Show"
                );
                if matches!(
                    item.tier0_delivery,
                    Tier0ToastDelivery::StartedUnknown { .. }
                ) {
                    let live_claim = live_claim_for_started.take().ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "Tier-0 committed StartedUnknown without its pre-commit live-claim reservation: escalation_id={escalation_id} committed_seq={committed_seq}"
                            ),
                        )
                    })?;
                    return Ok(Tier0ToastClaim::Started(item, live_claim));
                }
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                code = "ESCALATION_TIER0_CLAIM_REVISION_RETRY",
                escalation_id,
                revision_attempt,
                observed_seq,
                "Tier-0 claim lost an item/projection revision race; rereading before side effects"
            ),
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "Tier-0 toast {escalation_id} could not acquire stable item/projection revisions after {ACK_REVISION_MAX_ATTEMPTS} attempts"
        ),
    ))
}

fn restore_known_unsent_before_show_locked(
    db: &Db,
    escalation_id: &str,
    reason: &str,
) -> Result<(), ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let current = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 pre-Show cancellation reconciliation lost escalation item {escalation_id}; reason={reason}"
                ),
            )
        })?;
        if current.item.status != EscalationStatus::Pending {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 pre-Show cancellation observed terminal state despite retaining the transition lock: escalation_id={escalation_id} status={} reason={reason}",
                    current.item.status.as_str()
                ),
            ));
        }
        let (tag, projection_generation, started_at_unix_ms) = match &current.item.tier0_delivery {
            Tier0ToastDelivery::StartedUnknown {
                tag,
                projection_generation,
                started_at_unix_ms,
                ..
            } => (tag.clone(), *projection_generation, *started_at_unix_ms),
            other => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Tier-0 pre-Show cancellation reconciliation expected StartedUnknown: escalation_id={escalation_id} actual={other:?} reason={reason}"
                    ),
                ));
            }
        };
        let now_unix_ms = unix_time_ms_now();
        let mut updated = current.item;
        updated.tier0_delivery = Tier0ToastDelivery::KnownUnsent {
            tag: tag.clone(),
            projection_generation,
            prepared_at_unix_ms: started_at_unix_ms,
        };
        updated.updated_at_unix_ms = now_unix_ms.max(updated.updated_at_unix_ms);
        match write_item_and_audit_if_revision(
            db,
            &updated,
            "tier0_pre_show_cancelled",
            json!({
                "tag": tag,
                "group": SYNAPSE_ESCALATION_TOAST_GROUP,
                "reason": reason,
                "expires_at_unix_ms": updated.expires_at_unix_ms,
                "observed_at_unix_ms": now_unix_ms,
                "side_effect_started": false,
                "state": "known_unsent",
            }),
            current.revision_sha256,
        )? {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "Tier-0 pre-Show cancellation item {escalation_id} disappeared after committed_seq={committed_seq}; reason={reason}"
                        ),
                    )
                })?;
                if readback.item.tier0_delivery != updated.tier0_delivery {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 pre-Show cancellation readback differed: escalation_id={escalation_id} expected={:?} actual={:?} committed_seq={committed_seq} reason={reason}",
                            updated.tier0_delivery, readback.item.tier0_delivery
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_TIER0_PRE_SHOW_CANCELLED_DURABLE",
                    escalation_id,
                    revision_attempt,
                    committed_seq,
                    reason,
                    "readback=CF_KV known-unsent state after the COM worker proved Show was not invoked"
                );
                return Ok(());
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                code = "ESCALATION_TIER0_PRE_SHOW_CANCELLED_REVISION_RETRY",
                escalation_id,
                revision_attempt,
                observed_seq,
                reason,
                "Tier-0 pre-Show cancellation reconciliation raced another item update"
            ),
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "Tier-0 pre-Show cancellation for {escalation_id} could not acquire a stable item revision after {ACK_REVISION_MAX_ATTEMPTS} attempts; reason={reason}"
        ),
    ))
}

/// Frozen renderer used only to prove and migrate schema-v1 payload bytes. It
/// intentionally preserves the historical character-level truncation contract,
/// including a possible partial `[U+NNNN]` token at the boundary. New payloads
/// must use `tier0_project_text` (renderer v2) below.
fn tier0_project_text_v1(label: &str, value: &str, max_chars: usize) -> String {
    let original_chars = value.chars().count();
    let mut escaped_codepoints = 0_usize;
    let mut xml_safe = String::with_capacity(value.len());
    for character in value.chars() {
        let scalar = character as u32;
        if toast_text_char_allowed(character) {
            xml_safe.push(character);
        } else {
            use std::fmt::Write as _;
            let _ = write!(xml_safe, "[U+{scalar:04X}]");
            escaped_codepoints += 1;
        }
    }
    if escaped_codepoints == 0 && original_chars <= max_chars {
        return value.to_owned();
    }
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-tier0-text-projection-v1\0");
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    let source_sha256 = hex_bytes(&hasher.finalize());
    let marker = format!(
        "[… projected; original_chars={original_chars}; escaped_codepoints={escaped_codepoints}; source_sha256={source_sha256}]"
    );
    let keep_chars = max_chars.saturating_sub(marker.chars().count());
    let mut projected = xml_safe.chars().take(keep_chars).collect::<String>();
    projected.push_str(&marker);
    tracing::info!(
        code = "ESCALATION_TIER0_TEXT_PROJECTED_V1_MIGRATION",
        label,
        original_chars,
        escaped_codepoints,
        projected_chars = projected.chars().count(),
        source_sha256,
        renderer_version = TOAST_RENDERER_VERSION_V1,
        "frozen renderer v1 reproduced historical Tier-0 projection bytes for exact payload migration only"
    );
    projected
}

fn tier0_project_text(label: &str, value: &str, max_chars: usize) -> String {
    let mut original_chars = 0_usize;
    let mut escaped_codepoints = 0_usize;
    for character in value.chars() {
        original_chars += 1;
        if !toast_text_char_allowed(character) {
            escaped_codepoints += 1;
        }
    }
    if escaped_codepoints == 0 && original_chars <= max_chars {
        return value.to_owned();
    }
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-tier0-text-projection-v1\0");
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    let source_sha256 = hex_bytes(&hasher.finalize());
    let marker = format!(
        "[… projected; original_chars={original_chars}; escaped_codepoints={escaped_codepoints}; source_sha256={source_sha256}]"
    );
    let marker_chars = marker.chars().count();
    let keep_chars = max_chars.saturating_sub(marker_chars);
    let mut projected = String::with_capacity(max_chars);
    let mut projected_chars = 0_usize;
    for character in value.chars() {
        let token = if toast_text_char_allowed(character) {
            character.to_string()
        } else {
            format!("[U+{:04X}]", character as u32)
        };
        let token_chars = token.chars().count();
        if projected_chars.saturating_add(token_chars) > keep_chars {
            break;
        }
        projected.push_str(&token);
        projected_chars += token_chars;
    }
    projected.push_str(&marker);
    tracing::info!(
        code = "ESCALATION_TIER0_TEXT_PROJECTED",
        label,
        original_chars,
        escaped_codepoints,
        projected_chars = projected.chars().count(),
        source_sha256,
        renderer_version = TOAST_RENDERER_VERSION_CURRENT,
        digest_domain = "synapse-tier0-text-projection-v1",
        "Tier-0 toast uses a bounded deterministic projection; source_sha256 binds the full source and full context remains durable in CF_KV"
    );
    projected
}

fn tier0_notify_params(item: &EscalationItem) -> NotifyHumanParams {
    let title_prefix = "Synapse: ";
    let title_suffix = format!(" [{}]", item.severity.as_str());
    let action_chars = MAX_TITLE_CHARS
        .saturating_sub(title_prefix.chars().count())
        .saturating_sub(title_suffix.chars().count());
    let action = tier0_project_text("title_action", &item.context.action, action_chars);
    let title = format!("{title_prefix}{action}{title_suffix}");

    let anchor = tier0_project_text("anchor", &item.anchor, 256);
    let identity = format!("Agent: {anchor}\nEscalation: {}", item.escalation_id);
    let mut context = format!("Reason: {}", item.context.reason);
    if let Some(waiting) = &item.context.waiting_for {
        context.push_str("\nWaiting on: ");
        context.push_str(waiting);
    }
    let context_chars = MAX_BODY_CHARS
        .saturating_sub(identity.chars().count())
        .saturating_sub(1);
    let context = tier0_project_text("reason_waiting_context", &context, context_chars);
    let body = format!("{identity}\n{context}");
    NotifyHumanParams {
        title,
        body,
        kind: item.severity.notify_kind(),
        // Dedupe on the escalation id so repeated sweeps before dismissal do not
        // stack duplicate toasts.
        dedupe_key: Some(escalation_toast_dedupe_key(&item.escalation_id)),
        suppress_popup: item.tier0_quiet_digest,
    }
}

fn tier0_notify_params_v1(item: &EscalationItem) -> NotifyHumanParams {
    let title_prefix = "Synapse: ";
    let title_suffix = format!(" [{}]", item.severity.as_str());
    let action_chars = MAX_TITLE_CHARS
        .saturating_sub(title_prefix.chars().count())
        .saturating_sub(title_suffix.chars().count());
    let action = tier0_project_text_v1("title_action", &item.context.action, action_chars);
    let title = format!("{title_prefix}{action}{title_suffix}");

    let anchor = tier0_project_text_v1("anchor", &item.anchor, 256);
    let identity = format!("Agent: {anchor}\nEscalation: {}", item.escalation_id);
    let mut context = format!("Reason: {}", item.context.reason);
    if let Some(waiting) = &item.context.waiting_for {
        context.push_str("\nWaiting on: ");
        context.push_str(waiting);
    }
    let context_chars = MAX_BODY_CHARS
        .saturating_sub(identity.chars().count())
        .saturating_sub(1);
    let context = tier0_project_text_v1("reason_waiting_context", &context, context_chars);
    let body = format!("{identity}\n{context}");
    NotifyHumanParams {
        title,
        body,
        kind: item.severity.notify_kind(),
        dedupe_key: Some(escalation_toast_dedupe_key(&item.escalation_id)),
        suppress_popup: item.tier0_quiet_digest,
    }
}

/// Recreates the exact payload template shipped before the reserved Tier-0
/// namespace migration. Legacy rows are never trusted by Tag+Group alone: the
/// canonical WinRT digest of this frozen template must bind physical history.
fn legacy_tier0_notify_params(item: &EscalationItem) -> NotifyHumanParams {
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
    NotifyHumanParams {
        title,
        body,
        kind: item.severity.notify_kind(),
        dedupe_key: Some(escalation_toast_dedupe_key(&item.escalation_id)),
        suppress_popup: item.tier0_quiet_digest,
    }
}

fn tier0_notify_params_for_state(item: &EscalationItem) -> NotifyHumanParams {
    let legacy_tag = legacy_escalation_toast_tag(&item.escalation_id);
    if item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified
        || tier0_delivery_tag(&item.tier0_delivery) == Some(legacy_tag.as_str())
    {
        legacy_tier0_notify_params(item)
    } else {
        tier0_notify_params(item)
    }
}

fn tier0_notify_params_for_prepared(
    item: &EscalationItem,
    prepared: &PreparedToastPayload,
) -> Option<NotifyHumanParams> {
    let legacy_tag = legacy_escalation_toast_tag(&item.escalation_id);
    if item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified
        || tier0_delivery_tag(&item.tier0_delivery) == Some(legacy_tag.as_str())
    {
        return Some(legacy_tier0_notify_params(item));
    }
    match prepared.renderer_version {
        TOAST_RENDERER_VERSION_V1 => Some(tier0_notify_params_v1(item)),
        TOAST_RENDERER_VERSION_CURRENT => Some(tier0_notify_params(item)),
        _ => None,
    }
}

async fn prepare_tier0_payload(item: &EscalationItem) -> Result<PreparedToastPayload, ErrorData> {
    let params = tier0_notify_params_for_state(item);
    let prepared = prepare_internal_escalation_toast(params.clone(), Vec::new()).await?;
    if !prepared_toast_payload_valid(&prepared)
        || !prepared_toast_payload_matches_request(&prepared, &params, &[])
        || !is_canonical_sha256(&prepared.payload_sha256)
        || prepared.suppress_popup != item.tier0_quiet_digest
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 WinRT preparation returned an invalid frozen payload for escalation {}: schema_version={} xml_bytes={} suppress_popup={} payload_sha256={:?}",
                item.escalation_id,
                prepared.schema_version,
                prepared.canonical_xml.len(),
                prepared.suppress_popup,
                prepared.payload_sha256
            ),
        ));
    }
    Ok(prepared)
}

fn fire_tier0_blocking(
    item: &EscalationItem,
    frozen_payload: PreparedToastPayload,
    pre_show_authorizer: ToastPreShowAuthorizer,
) -> Result<Option<NotifyHumanResponse>, ErrorData> {
    let params = tier0_notify_params_for_prepared(item, &frozen_payload).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 frozen payload selected an unsupported renderer: escalation_id={} schema_version={} renderer_version={} payload_sha256={}",
                item.escalation_id,
                frozen_payload.schema_version,
                frozen_payload.renderer_version,
                frozen_payload.payload_sha256
            ),
        )
    })?;
    run_internal_escalation_toast_blocking(
        params,
        escalation_toast_tag(&item.escalation_id),
        Vec::new(),
        frozen_payload,
        item.expires_at_unix_ms,
        pre_show_authorizer,
    )
}

fn escalation_toast_dedupe_key(escalation_id: &str) -> String {
    format!("escalation:{escalation_id}")
}

fn escalation_toast_tag(escalation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-tier0-toast-tag-v1\0");
    hasher.update(escalation_id.as_bytes());
    let digest = hasher.finalize();
    let mut tag = String::with_capacity(36);
    tag.push_str("et1-");
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(tag, "{byte:02x}");
    }
    tag
}

fn legacy_escalation_toast_tag(escalation_id: &str) -> String {
    toast_tag_for(Some(&escalation_toast_dedupe_key(escalation_id)))
}

fn validate_tier0_send_response(
    item: &EscalationItem,
    response: &NotifyHumanResponse,
) -> Result<(), ErrorData> {
    let expected_tag = escalation_toast_tag(&item.escalation_id);
    let expected_payload = item.tier0_payload_sha256.as_deref().ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 send response cannot be validated without a durable payload digest: escalation_id={}",
                item.escalation_id
            ),
        )
    })?;
    let disposition_valid = response.shown != response.deduped;
    // The physical Action Center expiration must be either the durable deadline
    // or a provable Windows retention-cap truncation of it (#1803). Anything
    // else fails the contract (fail-closed) exactly as before.
    let now_unix_ms = checked_unix_time_ms("Tier-0 send-response validation boundary")?;
    let expiration_contract_satisfied = match response.expiration_unix_ms {
        Some(stored) => platform_corrected_expiration(
            item.expires_at_unix_ms,
            stored,
            item.created_at_unix_ms,
            now_unix_ms,
        )
        .is_some(),
        None => false,
    };
    if response.aumid != SYNAPSE_AUMID
        || response.group != SYNAPSE_ESCALATION_TOAST_GROUP
        || response.tag != expected_tag
        || !response.verified_in_history
        || response.history_count != 1
        || response.payload_sha256 != expected_payload
        || !expiration_contract_satisfied
        || !disposition_valid
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "Tier-0 WinRT response violated its physical-proof contract: escalation_id={} expected_tag={expected_tag} expected_payload_sha256={expected_payload} expected_expiration_unix_ms={} aumid={:?} group={:?} tag={:?} shown={} deduped={} verified_in_history={} history_count={} payload_sha256={:?} expiration_unix_ms={:?}",
                item.escalation_id,
                item.expires_at_unix_ms,
                response.aumid,
                response.group,
                response.tag,
                response.shown,
                response.deduped,
                response.verified_in_history,
                response.history_count,
                response.payload_sha256,
                response.expiration_unix_ms
            ),
        ));
    }
    Ok(())
}

fn error_data_symbol(error: &ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::NOTIFY_DELIVERY_UNVERIFIED)
        .to_owned()
}

fn legacy_pre_show_contract_rejection(error: &ErrorData) -> bool {
    matches!(
        error_data_symbol(error).as_str(),
        error_codes::TOOL_PARAMS_INVALID | error_codes::NOTIFY_XML_PAYLOAD_INVALID
    )
}

fn migrate_rejected_legacy_tier0_to_reserved(
    db: &Db,
    escalation_id: &str,
    legacy_history: &ToastHistoryReadback,
    preparation_error: &ErrorData,
) -> Result<(), ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| {
        for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
            let current = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "legacy Tier-0 contract migration lost escalation item {escalation_id}"
                    ),
                )
            })?;
            if current.item.tier0_delivery != Tier0ToastDelivery::LegacyUnclassified {
                return Ok(());
            }
            if current.item.status != EscalationStatus::Pending {
                return Ok(());
            }
            if current.item.tier0_fired
                || current.item.tier0_toast_removed.is_some()
                || current.item.tier0_payload_sha256.is_some()
                || current.item.tier0_prepared_payload.is_some()
            {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "legacy Tier-0 contract rejection cannot prove KnownUnsent because compatibility fields claim prior delivery/removal/binding: escalation_id={escalation_id} fired={} removal_present={} digest_present={} payload_present={}",
                        current.item.tier0_fired,
                        current.item.tier0_toast_removed.is_some(),
                        current.item.tier0_payload_sha256.is_some(),
                        current.item.tier0_prepared_payload.is_some()
                    ),
                ));
            }
            let mut updated = current.item;
            updated.tier0_delivery = Tier0ToastDelivery::NotRequested;
            updated.updated_at_unix_ms = checked_unix_time_ms(
                "legacy Tier-0 rejected-contract migration",
            )?
            .max(updated.updated_at_unix_ms);
            match write_item_and_audit_if_revision(
                db,
                &updated,
                "tier0_legacy_rejected_contract_migrated",
                json!({
                    "legacy_tag": legacy_history.tag,
                    "legacy_group": legacy_history.group,
                    "legacy_history": legacy_history,
                    "preparation_error_code": error_data_symbol(preparation_error),
                    "preparation_error_message": preparation_error.message.to_string(),
                    "proof": "the historical notify contract rejected before ToastNotifier.Show; reserved bounded rendering may start from NotRequested",
                    "legacy_row_mutation": "none",
                }),
                current.revision_sha256,
            )? {
                ItemWriteOutcome::Applied { committed_seq, .. } => {
                    let readback = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_READ_FAILED,
                            format!(
                                "legacy Tier-0 migration item disappeared after committed_seq={committed_seq}: escalation_id={escalation_id}"
                            ),
                        )
                    })?;
                    if readback.item.tier0_delivery != Tier0ToastDelivery::NotRequested
                        || readback.item.tier0_payload_sha256.is_some()
                        || readback.item.tier0_prepared_payload.is_some()
                    {
                        return Err(mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "legacy Tier-0 rejected-contract migration readback differed: escalation_id={escalation_id} delivery={:?} digest_present={} payload_present={} committed_seq={committed_seq}",
                                readback.item.tier0_delivery,
                                readback.item.tier0_payload_sha256.is_some(),
                                readback.item.tier0_prepared_payload.is_some()
                            ),
                        ));
                    }
                    return Ok(());
                }
                ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                    code = "ESCALATION_TIER0_LEGACY_REJECTED_MIGRATION_RETRY",
                    escalation_id,
                    revision_attempt,
                    observed_seq,
                    "legacy Tier-0 rejected-contract migration raced an item transition; rereading"
                ),
            }
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "legacy Tier-0 rejected-contract migration could not acquire a stable revision after {ACK_REVISION_MAX_ATTEMPTS} attempts: escalation_id={escalation_id}"
            ),
        ))
    })
    .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "legacy Tier-0 rejected-contract migration could not acquire the transition boundary: escalation_id={escalation_id} detail={detail}"
            ),
        )
    })?
}

fn classify_tier0_history(
    db: &Db,
    escalation_id: &str,
    readback: &ToastHistoryReadback,
    send_verified: bool,
    send_error: Option<&ErrorData>,
    now_unix_ms: u64,
) -> Result<Option<bool>, ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| {
        classify_tier0_history_locked(
            db,
            escalation_id,
            readback,
            send_verified,
            send_error,
            now_unix_ms,
        )
    })
    .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "Tier-0 history classification could not acquire the transition/ack boundary: escalation_id={escalation_id} detail={detail}"
            ),
        )
    })?
}

fn classify_tier0_history_locked(
    db: &Db,
    escalation_id: &str,
    readback: &ToastHistoryReadback,
    send_verified: bool,
    send_error: Option<&ErrorData>,
    now_unix_ms: u64,
) -> Result<Option<bool>, ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let current = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("Tier-0 history classification lost item {escalation_id}"),
            )
        })?;
        let (generation, guards, expected_tag) = match &current.item.tier0_delivery {
            Tier0ToastDelivery::StartedUnknown {
                tag,
                projection_generation,
                ..
            } => (
                *projection_generation,
                GuardedExtraRows::default(),
                tag.clone(),
            ),
            Tier0ToastDelivery::LegacyUnclassified => {
                if current.item.status != EscalationStatus::Pending {
                    return Ok(None);
                }
                let Some(projection) =
                    applied_projection_for_tier0_claim(db, &current.item, now_unix_ms)?
                else {
                    return Ok(None);
                };
                (
                    projection.record.observed.generation,
                    tier0_projection_guard(&current.item, &projection),
                    legacy_escalation_toast_tag(escalation_id),
                )
            }
            Tier0ToastDelivery::VerifiedPresent { .. }
            | Tier0ToastDelivery::VerifiedDismissed { .. } => return Ok(Some(true)),
            Tier0ToastDelivery::PreShowCollision { .. } => return Ok(Some(false)),
            Tier0ToastDelivery::Failed { .. } => return Ok(Some(false)),
            Tier0ToastDelivery::NotRequested
            | Tier0ToastDelivery::KnownUnsent { .. }
            | Tier0ToastDelivery::RemovalFailed { .. }
            | Tier0ToastDelivery::Removed { .. }
            | Tier0ToastDelivery::Suppressed { .. } => return Ok(None),
        };
        if !tier0_readback_identity_shape_valid(&current.item, &expected_tag, readback) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 Action Center readback identity/shape is invalid: escalation_id={} expected_tag={expected_tag} readback={readback:?}",
                    current.item.escalation_id
                ),
            ));
        }
        let expected_payload_sha256 = current
            .item
            .tier0_payload_sha256
            .clone()
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Tier-0 history classification lacks a durable payload binding: escalation_id={escalation_id}"
                    ),
                )
            })?;
        let payload_matches = readback.present
            && readback.history_count == 1
            && readback.payload_sha256s.first().map(String::as_str)
                == Some(expected_payload_sha256.as_str());
        let classified_at = checked_unix_time_ms("Tier-0 physical classification boundary")?
            .max(now_unix_ms)
            .max(current.item.updated_at_unix_ms);
        let single_present_row = readback.present && readback.history_count == 1;
        let stored_expiration_unix_ms = readback.expiration_unix_ms.first().copied().flatten();
        // Corrected expected expiration, reconciled against the Windows Action
        // Center retention cap (#1803): ordinary-TTL (7-day) toasts are stored
        // at `arrival + 3 days`, never at the durable deadline. Anchor the
        // verified expiration to the physically-stored value when the cap is
        // provably applied (arrival bounded by `[item created, now]`), keeping
        // exact-match semantics and the tag/group/payload identity guards.
        let mut platform_cap_applied = false;
        let (expected_expiration_unix_ms, expiration_matches) = match tier0_group_for_exact_tag(
            &current.item,
            &expected_tag,
        ) {
            Some(SYNAPSE_ESCALATION_TOAST_GROUP) => {
                let requested = current.item.expires_at_unix_ms;
                match (single_present_row, stored_expiration_unix_ms) {
                    (true, Some(stored)) => match platform_corrected_expiration(
                        requested,
                        stored,
                        current.item.created_at_unix_ms,
                        classified_at,
                    ) {
                        Some(corrected) => {
                            platform_cap_applied = corrected.platform_cap_applied;
                            (Some(corrected.expected_unix_ms), true)
                        }
                        None => (Some(requested), false),
                    },
                    _ => (Some(requested), false),
                }
            }
            Some(SYNAPSE_TOAST_GROUP) => (
                None,
                single_present_row && readback.expiration_unix_ms.first() == Some(&None),
            ),
            _ => {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Tier-0 classification has no expiration contract for escalation_id={escalation_id} tag={expected_tag}"
                    ),
                ));
            }
        };
        let physical_identity_matches = payload_matches && expiration_matches;
        let legacy_compatibility_delivery_proven = current.item.tier0_delivery
            == Tier0ToastDelivery::LegacyUnclassified
            && current.item.tier0_fired;
        let legacy_compatibility_verified_at = current.item.updated_at_unix_ms;
        let delivery_proven_before_classification = current.item.tier0_fired;
        let mut item = current.item;
        let delivery_proven;
        let delivery_proof_source;
        let event = if physical_identity_matches {
            delivery_proof_source = "separate Action Center exact payload and expiration readback";
            item.tier0_fired = true;
            item.tier0_delivery = Tier0ToastDelivery::VerifiedPresent {
                tag: readback.tag.clone(),
                projection_generation: generation,
                history_count: readback.history_count,
                verified_at_unix_ms: classified_at,
            };
            delivery_proven = true;
            "tier0_verified_present"
        } else if (send_verified || legacy_compatibility_delivery_proven) && !readback.present {
            delivery_proof_source = if send_verified {
                "current send exact Action Center delivery readback followed by separate absence"
            } else {
                "legacy tier0_fired compatibility proof followed by separate absence"
            };
            item.tier0_fired = true;
            item.tier0_delivery = Tier0ToastDelivery::VerifiedDismissed {
                tag: readback.tag.clone(),
                projection_generation: generation,
                history_count_at_delivery: 1,
                dismissed_readback: readback.clone(),
                verified_at_unix_ms: if send_verified {
                    classified_at
                } else {
                    legacy_compatibility_verified_at
                },
                dismissed_at_unix_ms: classified_at,
            };
            delivery_proven = true;
            if send_verified {
                "tier0_verified_then_dismissed"
            } else {
                "tier0_legacy_verified_then_dismissed"
            }
        } else {
            delivery_proof_source = if send_verified {
                "current send exact Action Center delivery proof retained; subsequent physical contract rejected"
            } else if delivery_proven_before_classification {
                "prior compatibility delivery proof retained; current physical contract rejected"
            } else {
                "no accepted delivery proof; current physical contract rejected"
            };
            let error_code = send_error.map_or_else(
                || error_codes::NOTIFY_DELIVERY_UNVERIFIED.to_owned(),
                error_data_symbol,
            );
            let physical_detail = if readback.present {
                format!(
                    "Action Center contained the expected Tag+Group identity with a different physical contract; expected_sha256={expected_payload_sha256} actual_sha256={:?} payload_matches={payload_matches} expected_expiration_unix_ms={expected_expiration_unix_ms:?} actual_expiration_unix_ms={:?} expiration_matches={expiration_matches}; the row is quarantined and will not be deleted as this escalation's toast",
                    readback.payload_sha256s, readback.expiration_unix_ms,
                )
            } else {
                "Action Center contained no matching Tag+Group row after a started or legacy-unclassified toast; it may have been displayed then dismissed, so automatic replay is unsafe".to_owned()
            };
            let error_message = send_error.map_or_else(
                || physical_detail.clone(),
                |error| format!("{}; {physical_detail}", error.message),
            );
            let delivery_proven_before_failure = item.tier0_fired || send_verified;
            item.tier0_fired = delivery_proven_before_failure;
            item.tier0_delivery = Tier0ToastDelivery::Failed {
                tag: readback.tag.clone(),
                projection_generation: Some(generation),
                error_code,
                error_message,
                delivery_proven_before_failure,
                side_effect_possible: true,
                history_readback: Some(readback.clone()),
                failed_at_unix_ms: classified_at,
            };
            delivery_proven = false;
            "tier0_delivery_failed"
        };
        item.updated_at_unix_ms = classified_at;
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            event,
            json!({
                "tag": readback.tag,
                "group": readback.group,
                "history_count": readback.history_count,
                "present": readback.present,
                "history_readback": readback,
                "payload_matches": payload_matches,
                "expiration_matches": expiration_matches,
                "requested_expiration_unix_ms": item.expires_at_unix_ms,
                "expected_expiration_unix_ms": expected_expiration_unix_ms,
                "stored_expiration_unix_ms": stored_expiration_unix_ms,
                "actual_expiration_unix_ms": readback.expiration_unix_ms,
                "platform_cap_applied": platform_cap_applied,
                "windows_action_center_max_history_ms": WINDOWS_ACTION_CENTER_MAX_HISTORY_MS,
                "send_verified_before_separate_read": send_verified,
                "delivery_proof_before_classification": delivery_proven_before_classification,
                "delivery_proof_after_classification": item.tier0_fired,
                "delivery_proof_source": delivery_proof_source,
                "send_error_code": send_error.map(error_data_symbol),
                "send_error_message": send_error.map(|error| error.message.to_string()),
                "physical_failure_message": match &item.tier0_delivery {
                    Tier0ToastDelivery::Failed { error_message, .. } => Some(error_message),
                    _ => None,
                },
                "source_of_truth": "Windows Action Center history",
            }),
            guards,
            current.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let actual = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "Tier-0 classified item {escalation_id} disappeared after committed_seq={committed_seq}"
                        ),
                    )
                })?;
                if actual.item.tier0_delivery != item.tier0_delivery
                    || actual.item.tier0_fired != item.tier0_fired
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "Tier-0 physical classification readback differed: escalation_id={escalation_id} expected={:?} actual={:?} committed_seq={committed_seq}",
                            item.tier0_delivery, actual.item.tier0_delivery
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_TIER0_PHYSICAL_STATE_DURABLE",
                    escalation_id,
                    event,
                    revision_attempt,
                    committed_seq,
                    history_count = readback.history_count,
                    present = readback.present,
                    requested_expiration_unix_ms = item.expires_at_unix_ms,
                    expected_expiration_unix_ms = ?expected_expiration_unix_ms,
                    stored_expiration_unix_ms = ?stored_expiration_unix_ms,
                    expiration_matches,
                    platform_cap_applied,
                    "readback=CF_KV Tier-0 physical Action Center classification is durable"
                );
                return Ok(Some(delivery_proven));
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => tracing::info!(
                code = "ESCALATION_TIER0_CLASSIFY_REVISION_RETRY",
                escalation_id,
                revision_attempt,
                observed_seq,
                "Tier-0 physical classification raced an item transition; rereading without repeating WinRT I/O"
            ),
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "Tier-0 physical classification for {escalation_id} could not acquire a stable item revision after {ACK_REVISION_MAX_ATTEMPTS} attempts"
        ),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tier0ProcessOutcome {
    Fired,
    Busy,
    ExpiredBeforeShow,
    NoAction,
}

async fn drive_tier0_delivery(
    db: &Arc<Db>,
    escalation_id: &str,
    _sweep_now_unix_ms: u64,
) -> Result<Tier0ProcessOutcome, ErrorData> {
    let mut preflight_item = read_item_revisioned(db, escalation_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("Tier-0 preflight has no escalation item {escalation_id}"),
        )
    })?;
    migrate_tier0_payload_v1_if_present(
        db,
        &mut preflight_item.item,
        &mut preflight_item.revision_sha256,
    )
    .await?;
    if preflight_item.item.status != EscalationStatus::Pending
        || preflight_item.item.tier0_suppressed_reason.is_some()
    {
        return Ok(Tier0ProcessOutcome::NoAction);
    }
    match &preflight_item.item.tier0_delivery {
        Tier0ToastDelivery::VerifiedPresent { .. }
        | Tier0ToastDelivery::VerifiedDismissed { .. }
        | Tier0ToastDelivery::PreShowCollision { .. }
        | Tier0ToastDelivery::Failed { .. }
        | Tier0ToastDelivery::RemovalFailed { .. }
        | Tier0ToastDelivery::Removed { .. }
        | Tier0ToastDelivery::Suppressed { .. } => return Ok(Tier0ProcessOutcome::NoAction),
        Tier0ToastDelivery::LegacyUnclassified
        | Tier0ToastDelivery::NotRequested
        | Tier0ToastDelivery::KnownUnsent { .. }
        | Tier0ToastDelivery::StartedUnknown { .. } => {}
    }
    let prepared_payload = match preflight_item.item.tier0_prepared_payload.clone() {
        Some(prepared)
            if prepared_toast_payload_valid(&prepared)
                && tier0_prepared_request_binding_valid(&preflight_item.item, &prepared)
                && prepared.suppress_popup == preflight_item.item.tier0_quiet_digest =>
        {
            prepared
        }
        Some(prepared) => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 durable frozen payload is invalid before delivery: escalation_id={escalation_id} schema_version={} xml_bytes={} suppress_popup={} payload_sha256={:?}",
                    prepared.schema_version,
                    prepared.canonical_xml.len(),
                    prepared.suppress_popup,
                    prepared.payload_sha256
                ),
            ));
        }
        None if preflight_item.item.tier0_delivery == Tier0ToastDelivery::LegacyUnclassified => {
            match prepare_tier0_payload(&preflight_item.item).await {
                Ok(prepared) => prepared,
                Err(error) if legacy_pre_show_contract_rejection(&error) => {
                    let legacy_history =
                        inspect_internal_toast(legacy_escalation_toast_tag(escalation_id)).await?;
                    migrate_rejected_legacy_tier0_to_reserved(
                        db,
                        escalation_id,
                        &legacy_history,
                        &error,
                    )?;
                    return Ok(Tier0ProcessOutcome::NoAction);
                }
                Err(error) => return Err(error),
            }
        }
        None if preflight_item.item.tier0_delivery == Tier0ToastDelivery::NotRequested => {
            prepare_tier0_payload(&preflight_item.item).await?
        }
        None => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 delivery state lacks its frozen payload: escalation_id={escalation_id} delivery={:?}",
                    preflight_item.item.tier0_delivery
                ),
            ));
        }
    };
    let history_only = matches!(
        &preflight_item.item.tier0_delivery,
        Tier0ToastDelivery::LegacyUnclassified | Tier0ToastDelivery::StartedUnknown { .. }
    );
    let execution = if history_only {
        let claim = super::agent_state::with_transition_pipeline_lock(|| {
            claim_tier0_toast_locked(db, escalation_id, Some(&prepared_payload))
        })
        .map_err(|detail| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "Tier-0 history-only reconciliation could not acquire the transition boundary: escalation_id={escalation_id} detail={detail}"
                ),
            )
        })??;
        Tier0ClaimExecution {
            claim,
            send_result: None,
            expired_before_show: false,
        }
    } else {
        claim_and_fire_tier0(Arc::clone(db), preflight_item.item, prepared_payload).await?
    };
    if execution.expired_before_show {
        return Ok(Tier0ProcessOutcome::ExpiredBeforeShow);
    }
    let (item, live_claim) = match execution.claim {
        Tier0ToastClaim::Started(item, live_claim) => (item, Some(live_claim)),
        Tier0ToastClaim::NeedsHistory(item) => (item, None),
        Tier0ToastClaim::OwnedByLiveCaller => return Ok(Tier0ProcessOutcome::Busy),
        Tier0ToastClaim::NoAction => return Ok(Tier0ProcessOutcome::NoAction),
    };
    let mut send_error = None;
    let mut send_verified = false;
    if let Some(result) = execution.send_result {
        match result {
            Ok(response) => {
                if let Err(error) = validate_tier0_send_response(&item, &response) {
                    send_error = Some(error);
                } else {
                    send_verified = true;
                }
            }
            Err(error) => send_error = Some(error),
        }
    }
    let (tag, group) = tier0_reconciliation_identity(&item)?;
    let history = match group {
        SYNAPSE_ESCALATION_TOAST_GROUP => inspect_internal_escalation_toast(tag).await?,
        SYNAPSE_TOAST_GROUP => inspect_internal_toast(tag).await?,
        _ => {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Tier-0 inspection selected unsupported group {group:?} for escalation {escalation_id}"
                ),
            ));
        }
    };
    let classified = classify_tier0_history(
        db,
        escalation_id,
        &history,
        send_verified,
        send_error.as_ref(),
        unix_time_ms_now(),
    )?;
    drop(live_claim);
    if let Some(error) = send_error {
        return Err(error);
    }
    match classified {
        Some(true) => Ok(Tier0ProcessOutcome::Fired),
        Some(false) => Err(mcp_error(
            error_codes::NOTIFY_DELIVERY_UNVERIFIED,
            format!(
                "Tier-0 toast {escalation_id} failed or had an ambiguous physical contract after its started/legacy boundary; durable state preserves the full Action Center readback and automatic replay is disabled"
            ),
        )),
        None => Ok(Tier0ProcessOutcome::NoAction),
    }
}

enum WebhookPostClaim {
    Claimed(WebhookOutboxRecord),
    ObservedPostStarted(WebhookOutboxRecord),
    OwnedByLiveCaller,
    Abandoned(String),
}

fn claim_webhook_post(
    db: &Db,
    intent: &WebhookOutboxRecord,
    now_unix_ms: u64,
    shutdown: &CancellationToken,
) -> Result<WebhookPostClaim, ErrorData> {
    super::agent_state::with_transition_pipeline_lock(|| {
        claim_webhook_post_inner(db, intent, now_unix_ms, shutdown)
    })
    .map_err(|detail| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "webhook POST claim could not acquire the agent transition linearization boundary: delivery_id={} detail={detail}",
                intent.delivery_id
            ),
        )
    })?
}

fn claim_webhook_post_inner(
    db: &Db,
    intent: &WebhookOutboxRecord,
    now_unix_ms: u64,
    shutdown: &CancellationToken,
) -> Result<WebhookPostClaim, ErrorData> {
    for revision_attempt in 1..=ACK_REVISION_MAX_ATTEMPTS {
        let current_item = read_item_revisioned(db, &intent.escalation_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook POST claim {} has no escalation item",
                    intent.delivery_id
                ),
            )
        })?;
        let current_outbox =
            read_outbox_revisioned(db, &intent.escalation_id, intent.ladder_index)?.ok_or_else(
                || {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "webhook POST claim {} has no outbox row",
                            intent.delivery_id
                        ),
                    )
                },
            )?;
        if current_outbox.record.delivery_id != intent.delivery_id
            || current_outbox.record.attempt_number != intent.attempt_number
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook POST claim identity changed: intended_delivery={} intended_attempt={} current_delivery={} current_attempt={}",
                    intent.delivery_id,
                    intent.attempt_number,
                    current_outbox.record.delivery_id,
                    current_outbox.record.attempt_number
                ),
            ));
        }
        let config_key = CONFIG_KEY.as_bytes().to_vec();
        let Some(config_revisioned) = db
            .get_cf_revisioned(cf::CF_KV, &config_key)
            .map_err(storage_error)?
        else {
            return Ok(WebhookPostClaim::Abandoned(
                "escalation policy disappeared before POST claim".to_owned(),
            ));
        };
        let config_value = live_revisioned_value(
            &config_revisioned,
            "escalation policy at webhook POST claim",
        )?;
        let current_policy = decode_json::<EscalationPolicy>(config_value).map_err(|error| {
            mcp_error(
                error.code(),
                format!("escalation policy decode failed at webhook POST claim: {error}"),
            )
        })?;
        validate_policy(&current_policy, true, error_codes::STORAGE_CORRUPTED)?;
        let current_channel = current_policy
            .webhooks
            .iter()
            .find(|channel| channel.channel_id == intent.channel_id)
            .cloned();
        let current_generation = current_policy.receiver_generations.get(&intent.channel_id);
        let receiver_identity_matches = current_channel.as_ref().is_some_and(|channel| {
            let name_matches = channel.name == intent.channel_name;
            let contract_matches = channel.idempotency_contract == intent.idempotency_contract;
            let host_matches = webhook_url_host(&channel.url) == intent.url_host;
            let fingerprint_matches =
                webhook_channel_fingerprint(channel) == intent.channel_fingerprint_sha256;
            let signing_matches = channel.secret.is_some() == intent.signed;
            name_matches
                && contract_matches
                && host_matches
                && fingerprint_matches
                && signing_matches
        });
        let receiver_generation_matches =
            current_generation.is_some_and(|generation| generation == &intent.receiver_generation);
        let channel_matches = receiver_identity_matches && receiver_generation_matches;
        if !channel_matches {
            return Ok(WebhookPostClaim::Abandoned(format!(
                "stable webhook channel {:?} or its opaque receiver generation no longer matches the durable intent before POST claim",
                intent.channel_id
            )));
        }
        if current_outbox.record.state == WebhookOutboxState::PostStarted {
            if current_outbox.record.post_started_owner_epoch.as_deref() == Some(daemon_epoch()) {
                return Ok(WebhookPostClaim::OwnedByLiveCaller);
            }
            return Ok(WebhookPostClaim::ObservedPostStarted(current_outbox.record));
        }
        if current_outbox.record.state != WebhookOutboxState::InFlight {
            return Ok(WebhookPostClaim::Abandoned(format!(
                "outbox left pre-POST state before claim: state={:?}",
                current_outbox.record.state
            )));
        }
        if current_item.item.status != EscalationStatus::Pending
            || !current_item.item.tier1_eligible
            || current_item.item.ladder_index != intent.ladder_index
        {
            return Ok(WebhookPostClaim::Abandoned(format!(
                "escalation stopped before POST claim: status={} tier1_eligible={} item_ladder_index={} delivery_ladder_index={}",
                current_item.item.status.as_str(),
                current_item.item.tier1_eligible,
                current_item.item.ladder_index,
                intent.ladder_index
            )));
        }
        if current_outbox.record != *intent {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook in-flight outbox changed without changing delivery identity or attempt: delivery_id={} attempt_number={}",
                    intent.delivery_id, intent.attempt_number
                ),
            ));
        }
        let current_channel = current_channel.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "webhook channel disappeared after receiver identity validation: channel_id={:?}",
                    intent.channel_id
                ),
            )
        })?;
        let expected_body_json = serde_json::to_string(&webhook_payload(
            &current_channel,
            &current_item.item,
        ))
        .map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "authoritative webhook payload could not be encoded before POST claim: delivery_id={} error={error}",
                    intent.delivery_id
                ),
            )
        })?;
        let expected_body_sha256 = hex_bytes(&Sha256::digest(expected_body_json.as_bytes()));
        if intent.body_json != expected_body_json || intent.body_sha256 != expected_body_sha256 {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "durable webhook payload does not exactly match the guarded escalation item: delivery_id={} body_matches={} digest_matches={}",
                    intent.delivery_id,
                    intent.body_json == expected_body_json,
                    intent.body_sha256 == expected_body_sha256
                ),
            ));
        }
        let authoritative_reads = super::agent_state::reads(now_unix_ms);
        let Some(authoritative_agent) =
            authoritative_agent_read_for_item(&authoritative_reads, &current_item.item)
        else {
            return Ok(WebhookPostClaim::Abandoned(format!(
                "authoritative agent state is absent at POST claim for anchor {:?}",
                current_item.item.anchor
            )));
        };
        if authoritative_agent.state.as_str() != current_item.item.attention_state
            || authoritative_agent.state == AgentLifecycleState::Dead
        {
            return Ok(WebhookPostClaim::Abandoned(format!(
                "authoritative agent state changed before POST claim: item_state={:?} agent_state={:?}",
                current_item.item.attention_state,
                authoritative_agent.state.as_str()
            )));
        }
        let projection =
            read_projection_watermark(db, &current_item.item.anchor)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "webhook POST claim has no transition projection cursor: anchor={:?}",
                        current_item.item.anchor
                    ),
                )
            })?;
        if projection.record.phase != TransitionProjectionPhase::Applied
            || projection.record.observed.anchor != current_item.item.anchor
            || projection.record.observed.state_to != current_item.item.attention_state
        {
            return Ok(WebhookPostClaim::Abandoned(format!(
                "transition projection is not the exact Applied attention state at POST claim: phase={:?} cursor_state={:?} item_state={:?}",
                projection.record.phase,
                projection.record.observed.state_to,
                current_item.item.attention_state
            )));
        }
        verify_applied_projection_evidence(db, &projection.record)?;
        let projection_application = projection
            .record
            .applied_evidence
            .as_ref()
            .map(|evidence| &evidence.application)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "Applied transition projection has no evidence at webhook claim: anchor={:?}",
                        current_item.item.anchor
                    ),
                )
            })?;
        if !matches!(
            projection_application,
            TransitionProjectionApplication::Escalation {
                escalation_id,
                approval_id,
            } if escalation_id == &current_item.item.escalation_id
                && approval_id == &current_item.item.approval_id
        ) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "Applied projection evidence is not bound to the escalation/approval claimed for webhook delivery: escalation_id={} approval_id={}",
                    current_item.item.escalation_id, current_item.item.approval_id
                ),
            ));
        }
        let claim_now_unix_ms = checked_unix_time_ms("webhook exact pre-POST claim boundary")?;
        if claim_now_unix_ms >= current_item.item.expires_at_unix_ms {
            return Ok(WebhookPostClaim::Abandoned(format!(
                "escalation expired at the exact pre-POST claim boundary: now_unix_ms={claim_now_unix_ms} expires_at_unix_ms={}",
                current_item.item.expires_at_unix_ms
            )));
        }
        if shutdown.is_cancelled() {
            return Ok(WebhookPostClaim::Abandoned(
                "shutdown linearized before the durable POST claim; POST was not sent".to_owned(),
            ));
        }
        let claimed_at = claim_now_unix_ms
            .max(current_outbox.record.attempt_started_at_unix_ms)
            .max(current_outbox.record.updated_at_unix_ms);
        let mut item = current_item.item;
        item.updated_at_unix_ms = claimed_at;
        let mut claimed = current_outbox.record;
        claimed.state = WebhookOutboxState::PostStarted;
        claimed.contract_verified = true;
        claimed.updated_at_unix_ms = claimed_at;
        claimed.post_started_owner_epoch = Some(daemon_epoch().to_owned());
        let projection_key = projection_watermark_key(&item.anchor);
        let mut claim_rows = outbox_row(&claimed, Some(current_outbox.revision_sha256))?;
        claim_rows.extend(GuardedExtraRows {
            rows: Vec::new(),
            guards: vec![RevisionGuard::new(
                projection_key,
                Some(projection.revision_sha256),
            )],
        });
        claim_rows.extend(GuardedExtraRows {
            rows: Vec::new(),
            guards: vec![RevisionGuard::new(
                config_key,
                Some(config_revisioned.revision_sha256),
            )],
        });
        let outcome = write_item_and_audit_with_extra_rows_if_revision(
            db,
            &item,
            "tier1_post_claimed",
            json!({
                "delivery_id": claimed.delivery_id,
                "channel_name": claimed.channel_name,
                "url_host": claimed.url_host,
                "ladder_index": claimed.ladder_index,
                "attempt_number": claimed.attempt_number,
                "idempotency_contract": claimed.idempotency_contract.as_str(),
                "outbox_state": "post_started",
                "remediation": "a crash after this boundary is an unknown remote outcome and may retry only with the same delivery ID",
            }),
            claim_rows,
            current_item.revision_sha256,
        )?;
        match outcome {
            ItemWriteOutcome::Applied { committed_seq, .. } => {
                let readback =
                    read_outbox_revisioned(db, &intent.escalation_id, intent.ladder_index)?
                        .ok_or_else(|| {
                            mcp_error(
                                error_codes::STORAGE_READ_FAILED,
                                format!(
                                    "claimed webhook outbox {} disappeared",
                                    intent.delivery_id
                                ),
                            )
                        })?;
                if readback.record != claimed {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "webhook POST claim readback was not byte-equivalent to this caller's committed claim: delivery_id={} state={:?} attempt={}",
                            intent.delivery_id,
                            readback.record.state,
                            readback.record.attempt_number
                        ),
                    ));
                }
                tracing::info!(
                    code = "ESCALATION_WEBHOOK_POST_CLAIMED",
                    escalation_id = %intent.escalation_id,
                    delivery_id = %intent.delivery_id,
                    ladder_index = intent.ladder_index,
                    attempt_number = intent.attempt_number,
                    committed_seq,
                    outbox_revision = %hex_bytes(&readback.revision_sha256),
                    "readback=CF_KV pending item and post-started outbox CAS committed immediately before POST"
                );
                return Ok(WebhookPostClaim::Claimed(readback.record));
            }
            ItemWriteOutcome::Conflict { observed_seq, .. } => {
                tracing::info!(
                    code = "ESCALATION_WEBHOOK_POST_CLAIM_RETRY",
                    escalation_id = %intent.escalation_id,
                    delivery_id = %intent.delivery_id,
                    attempt_number = intent.attempt_number,
                    revision_attempt,
                    observed_seq,
                    "pre-POST item/outbox claim raced acknowledgment or another mutation; rereading before any POST"
                );
            }
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "webhook POST claim {} could not acquire stable item/outbox revisions after {ACK_REVISION_MAX_ATTEMPTS} attempts; POST was not sent",
            intent.delivery_id
        ),
    ))
}

async fn deliver_webhook(
    db: &Db,
    channel: &WebhookChannel,
    outbox: &WebhookOutboxRecord,
    shutdown: &CancellationToken,
) -> Result<Option<WebhookDeliveryResult>, ErrorData> {
    let mut result = WebhookDeliveryResult {
        attempt: ChannelAttempt {
            delivery_id: outbox.delivery_id.clone(),
            channel_id: outbox.channel_id.clone(),
            channel_name: outbox.channel_name.clone(),
            url_host: outbox.url_host.clone(),
            ladder_index: outbox.ladder_index,
            attempt_number: outbox.attempt_number,
            outcome: WebhookAttemptOutcome::Unknown,
            ok: false,
            http_status: None,
            error: None,
            signed: outbox.signed,
            at_unix_ms: unix_time_ms_now()
                .max(outbox.attempt_started_at_unix_ms)
                .max(outbox.updated_at_unix_ms),
        },
        state: WebhookOutboxState::Unknown,
        contract_verified: false,
        response_delivery_id: None,
        response_body_sha256: None,
        response_receipt_state: None,
    };
    if outbox.idempotency_contract != WebhookIdempotencyContract::SynapseReceiptV1 {
        result.attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
        result.state = WebhookOutboxState::TerminalFailure;
        result.attempt.error = Some(
            "historical webhook has no durable receiver receipt contract; network I/O refused—rewrite escalation policy with idempotency_contract=synapse_receipt_v1 after upgrading the receiver"
                .to_owned(),
        );
        return Ok(Some(result));
    }
    let actual_fingerprint = webhook_channel_fingerprint(channel);
    if actual_fingerprint != outbox.channel_fingerprint_sha256
        || channel.channel_id != outbox.channel_id
        || channel.name != outbox.channel_name
        || channel.idempotency_contract != outbox.idempotency_contract
        || webhook_url_host(&channel.url) != outbox.url_host
    {
        result.attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
        result.state = WebhookOutboxState::TerminalFailure;
        result.attempt.error = Some(format!(
            "configured webhook identity changed after durable intent: expected_fingerprint={} actual_fingerprint={}; create a new escalation generation after correcting policy",
            outbox.channel_fingerprint_sha256, actual_fingerprint
        ));
        return Ok(Some(result));
    }
    let body_digest = hex_bytes(&Sha256::digest(outbox.body_json.as_bytes()));
    if body_digest != outbox.body_sha256 {
        result.attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
        result.state = WebhookOutboxState::TerminalFailure;
        result.attempt.error = Some(format!(
            "durable webhook body digest mismatch: expected={} actual={}",
            outbox.body_sha256, body_digest
        ));
        return Ok(Some(result));
    }
    let signature_timestamp_ms = outbox.attempt_started_at_unix_ms;
    let signature = match &channel.secret {
        Some(secret) => {
            match webhook_signature_hex(secret, &channel.url, outbox, signature_timestamp_ms) {
                Ok(signature) => Some(signature),
                Err(error) => {
                    result.attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
                    result.state = WebhookOutboxState::TerminalFailure;
                    result.attempt.error = Some(format!(
                        "webhook signature construction failed before network I/O: {}",
                        error.message
                    ));
                    return Ok(Some(result));
                }
            }
        }
        None => None,
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(WEBHOOK_TIMEOUT_MS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            result.attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
            result.state = WebhookOutboxState::TerminalFailure;
            result.attempt.error = Some(format!(
                "http client build failed before network I/O: {}",
                safe_reqwest_error_context(&error)
            ));
            return Ok(Some(result));
        }
    };
    let protocol = outbox.idempotency_contract.as_str();
    let preflight = client
        .request(reqwest::Method::OPTIONS, &channel.url)
        .header(WEBHOOK_IDEMPOTENCY_PROTOCOL_HEADER, protocol)
        .header(WEBHOOK_DELIVERY_ID_HEADER, &outbox.delivery_id)
        .header(WEBHOOK_IDEMPOTENCY_KEY_HEADER, &outbox.delivery_id)
        .header(WEBHOOK_BODY_SHA256_HEADER, &outbox.body_sha256)
        .send()
        .await;
    let preflight = match preflight {
        Ok(response) => response,
        Err(error) => {
            result.attempt.outcome = WebhookAttemptOutcome::TransientFailure;
            result.state = WebhookOutboxState::TransientFailure;
            result.attempt.error = Some(format!(
                "idempotency preflight failed before POST; no receiver contract was established: {}",
                safe_reqwest_error_context(&error)
            ));
            result.attempt.at_unix_ms = unix_time_ms_now()
                .max(outbox.attempt_started_at_unix_ms)
                .max(outbox.updated_at_unix_ms);
            return Ok(Some(result));
        }
    };
    let preflight_status = preflight.status();
    result.attempt.http_status = Some(preflight_status.as_u16());
    let preflight_protocol_matches = response_header_matches(
        preflight.headers(),
        WEBHOOK_IDEMPOTENCY_PROTOCOL_HEADER,
        protocol,
    );
    let preflight_delivery_matches = response_header_matches(
        preflight.headers(),
        WEBHOOK_DELIVERY_ID_HEADER,
        &outbox.delivery_id,
    );
    let preflight_body_matches = response_header_matches(
        preflight.headers(),
        WEBHOOK_BODY_SHA256_HEADER,
        &outbox.body_sha256,
    );
    let preflight_receipt_ready = response_header_matches(
        preflight.headers(),
        WEBHOOK_RECEIPT_STATE_HEADER,
        WEBHOOK_RECEIPT_READY,
    );
    if !preflight_status.is_success() && webhook_status_retryable(preflight_status) {
        result.attempt.outcome = WebhookAttemptOutcome::TransientFailure;
        result.state = WebhookOutboxState::TransientFailure;
        result.response_delivery_id = None;
        result.response_body_sha256 = None;
        result.response_receipt_state = None;
        result.attempt.error = Some(format!(
            "idempotency preflight returned safely retryable status={}; POST was not sent",
            preflight_status.as_u16()
        ));
        result.attempt.at_unix_ms = unix_time_ms_now()
            .max(outbox.attempt_started_at_unix_ms)
            .max(outbox.updated_at_unix_ms);
        return Ok(Some(result));
    }
    if !preflight_status.is_success()
        || !preflight_protocol_matches
        || !preflight_delivery_matches
        || !preflight_body_matches
        || !preflight_receipt_ready
    {
        result.attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
        result.state = WebhookOutboxState::TerminalFailure;
        result.response_delivery_id = None;
        result.response_body_sha256 = None;
        result.response_receipt_state = None;
        result.attempt.error = Some(format!(
            "endpoint did not prove durable receipt contract before POST: status={preflight_status} protocol_match={preflight_protocol_matches} delivery_id_match={preflight_delivery_matches} body_sha256_match={preflight_body_matches} receipt_ready={preflight_receipt_ready}"
        ));
        result.attempt.at_unix_ms = unix_time_ms_now()
            .max(outbox.attempt_started_at_unix_ms)
            .max(outbox.updated_at_unix_ms);
        return Ok(Some(result));
    }
    result.contract_verified = true;
    if shutdown.is_cancelled() {
        result.attempt.outcome = WebhookAttemptOutcome::Abandoned;
        result.state = WebhookOutboxState::Abandoned;
        result.contract_verified = false;
        result.attempt.http_status = None;
        result.response_delivery_id = None;
        result.response_body_sha256 = None;
        result.response_receipt_state = None;
        result.attempt.error = Some(
            "shutdown was requested after idempotency preflight but before the durable POST claim; POST was not sent"
                .to_owned(),
        );
        result.attempt.at_unix_ms = unix_time_ms_now()
            .max(outbox.attempt_started_at_unix_ms)
            .max(outbox.updated_at_unix_ms);
        return Ok(Some(result));
    }
    let outbox = match claim_webhook_post(db, outbox, unix_time_ms_now(), shutdown)? {
        WebhookPostClaim::Claimed(claimed) => claimed,
        WebhookPostClaim::OwnedByLiveCaller => return Ok(None),
        WebhookPostClaim::ObservedPostStarted(observed) => {
            result.attempt.outcome = WebhookAttemptOutcome::Unknown;
            result.state = WebhookOutboxState::Unknown;
            result.contract_verified = true;
            result.attempt.http_status = None;
            result.response_delivery_id = None;
            result.response_body_sha256 = None;
            result.response_receipt_state = None;
            result.attempt.error = Some(format!(
                "another caller already committed the POST-started boundary for delivery_id={}; this caller did not send and the durable intent must be reconciled as unknown before any numbered retry",
                observed.delivery_id
            ));
            result.attempt.at_unix_ms = unix_time_ms_now()
                .max(observed.attempt_started_at_unix_ms)
                .max(observed.updated_at_unix_ms);
            return Ok(Some(result));
        }
        WebhookPostClaim::Abandoned(reason) => {
            result.attempt.outcome = WebhookAttemptOutcome::Abandoned;
            result.state = WebhookOutboxState::Abandoned;
            result.contract_verified = false;
            result.attempt.http_status = None;
            result.response_delivery_id = None;
            result.response_body_sha256 = None;
            result.response_receipt_state = None;
            result.attempt.error = Some(format!(
                "receiver contract was proven, but POST was not sent: {reason}"
            ));
            result.attempt.at_unix_ms = unix_time_ms_now()
                .max(outbox.attempt_started_at_unix_ms)
                .max(outbox.updated_at_unix_ms);
            return Ok(Some(result));
        }
    };
    // Preflight response metadata is not POST outcome evidence. Once this
    // caller owns the durable PostStarted CAS, clear it before observing POST.
    result.attempt.http_status = None;
    result.response_delivery_id = None;
    result.response_body_sha256 = None;
    result.response_receipt_state = None;
    let mut request = client
        .post(&channel.url)
        .header("Content-Type", "application/json")
        .header(WEBHOOK_IDEMPOTENCY_PROTOCOL_HEADER, protocol)
        .header(WEBHOOK_DELIVERY_ID_HEADER, &outbox.delivery_id)
        .header(WEBHOOK_IDEMPOTENCY_KEY_HEADER, &outbox.delivery_id)
        .header(WEBHOOK_BODY_SHA256_HEADER, &outbox.body_sha256);
    if let Some(signature) = signature {
        request = request
            .header(WEBHOOK_SIGNATURE_HEADER, format!("v2={signature}"))
            .header(
                WEBHOOK_SIGNATURE_TIMESTAMP_HEADER,
                signature_timestamp_ms.to_string(),
            )
            .header(WEBHOOK_SIGNATURE_AUDIENCE_HEADER, &channel.url)
            .header(
                WEBHOOK_SIGNATURE_MAX_AGE_HEADER,
                WEBHOOK_SIGNATURE_MAX_AGE_MS.to_string(),
            );
    }
    match request.body(outbox.body_json.clone()).send().await {
        Ok(response) => {
            let status = response.status();
            result.attempt.http_status = Some(status.as_u16());
            let response_protocol_matches = response_header_matches(
                response.headers(),
                WEBHOOK_IDEMPOTENCY_PROTOCOL_HEADER,
                protocol,
            );
            let response_delivery_matches = response_header_matches(
                response.headers(),
                WEBHOOK_DELIVERY_ID_HEADER,
                &outbox.delivery_id,
            );
            let response_body_matches = response_header_matches(
                response.headers(),
                WEBHOOK_BODY_SHA256_HEADER,
                &outbox.body_sha256,
            );
            let response_receipt_state = if response_header_matches(
                response.headers(),
                WEBHOOK_RECEIPT_STATE_HEADER,
                WEBHOOK_RECEIPT_COMMITTED,
            ) {
                Some(WebhookReceiptState::Committed)
            } else if response_header_matches(
                response.headers(),
                WEBHOOK_RECEIPT_STATE_HEADER,
                WEBHOOK_RECEIPT_NOT_COMMITTED,
            ) {
                Some(WebhookReceiptState::NotCommitted)
            } else {
                None
            };
            let expected_receipt_state = if status.is_success() {
                WebhookReceiptState::Committed
            } else {
                WebhookReceiptState::NotCommitted
            };
            if !response_protocol_matches
                || !response_delivery_matches
                || !response_body_matches
                || response_receipt_state != Some(expected_receipt_state)
            {
                result.attempt.outcome = WebhookAttemptOutcome::Unknown;
                result.state = WebhookOutboxState::UnknownTerminal;
                result.attempt.error = Some(format!(
                    "endpoint POST response violated the proven durable receipt contract: status={} protocol_match={} delivery_id_match={} body_sha256_match={} expected_receipt_state={} actual_receipt_state={:?}; outcome may include a remote side effect and will not be retried",
                    status,
                    response_protocol_matches,
                    response_delivery_matches,
                    response_body_matches,
                    expected_receipt_state.as_header(),
                    response_receipt_state
                ));
            } else {
                result.response_delivery_id = Some(outbox.delivery_id.clone());
                result.response_body_sha256 = Some(outbox.body_sha256.clone());
                result.response_receipt_state = response_receipt_state;
                if status.is_success() {
                    result.attempt.outcome = WebhookAttemptOutcome::Accepted;
                    result.state = WebhookOutboxState::Accepted;
                    result.attempt.ok = true;
                } else if webhook_status_retryable(status) {
                    result.attempt.outcome = WebhookAttemptOutcome::TransientFailure;
                    result.state = WebhookOutboxState::TransientFailure;
                    result.attempt.error = Some(format!(
                        "receipt-valid not_committed response: {status}; retry uses the same durable delivery ID"
                    ));
                } else {
                    result.attempt.outcome = WebhookAttemptOutcome::TerminalFailure;
                    result.state = WebhookOutboxState::TerminalFailure;
                    result.attempt.error = Some(format!(
                        "receipt-valid permanent not_committed response: {status}; receiver rejected the logical delivery and it will not be retried"
                    ));
                }
            }
        }
        Err(error) => {
            result.attempt.outcome = WebhookAttemptOutcome::Unknown;
            result.state = WebhookOutboxState::Unknown;
            result.attempt.error = Some(format!(
                "POST outcome unknown after receiver contract was verified: {}; retry is permitted only with delivery_id={}",
                safe_reqwest_error_context(&error),
                outbox.delivery_id
            ));
        }
    }
    result.attempt.at_unix_ms = unix_time_ms_now()
        .max(outbox.attempt_started_at_unix_ms)
        .max(outbox.updated_at_unix_ms);
    Ok(Some(result))
}

fn webhook_status_retryable(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 408 | 409 | 425 | 429)
}

fn webhook_status_u16_retryable(status: u16) -> bool {
    reqwest::StatusCode::from_u16(status).is_ok_and(webhook_status_retryable)
}

/// Persist only transport classification that cannot contain the configured
/// URL, query, userinfo, or any secret-bearing request context. In particular,
/// never format `reqwest::Error` itself: its Display representation may embed
/// the full request URL.
fn safe_reqwest_error_context(error: &reqwest::Error) -> String {
    let class = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_builder() {
        "builder"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_status() {
        "status"
    } else if error.is_request() {
        "request"
    } else {
        "transport_other"
    };
    match error.status() {
        Some(status) => format!("class={class} status={}", status.as_u16()),
        None => format!("class={class} status=none"),
    }
}

fn response_header_matches(
    headers: &reqwest::header::HeaderMap,
    name: &str,
    expected: &str,
) -> bool {
    let mut values = headers.get_all(name).iter();
    matches!(
        (values.next(), values.next()),
        (Some(value), None) if value.to_str().ok() == Some(expected)
    )
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

fn webhook_payload(channel: &WebhookChannel, item: &EscalationItem) -> WebhookPayload {
    WebhookPayload {
        schema: "synapse.escalation.v1".to_owned(),
        channel_id: channel.channel_id.clone(),
        channel: channel.name.clone(),
        escalation_id: item.escalation_id.clone(),
        severity: item.severity.as_str().to_owned(),
        attention_state: item.attention_state.clone(),
        anchor: item.anchor.clone(),
        spawn_id: item.spawn_id.clone(),
        session_id: item.session_id.clone(),
        reason_code: item.reason_code.clone(),
        ladder_index: item.ladder_index,
        created_at_unix_ms: item.created_at_unix_ms,
        context: item.context.clone(),
    }
}

fn webhook_signature_hex(
    secret: &str,
    target_url: &str,
    outbox: &WebhookOutboxRecord,
    signature_timestamp_ms: u64,
) -> Result<String, ErrorData> {
    let canonical_target = reqwest::Url::parse(target_url).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("validated webhook target could not be canonicalized for signing: {error}"),
        )
    })?;
    let timestamp = signature_timestamp_ms.to_string();
    let max_age = WEBHOOK_SIGNATURE_MAX_AGE_MS.to_string();
    let mut envelope = Vec::new();
    envelope.extend_from_slice(b"synapse.webhook.signature.v2\0");
    for value in [
        b"POST".as_slice(),
        canonical_target.as_str().as_bytes(),
        outbox.idempotency_contract.as_str().as_bytes(),
        outbox.delivery_id.as_bytes(),
        timestamp.as_bytes(),
        max_age.as_bytes(),
        outbox.body_sha256.as_bytes(),
        outbox.body_json.as_bytes(),
    ] {
        let value_len = u64::try_from(value.len()).map_err(|_| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "webhook signature component exceeded the u64 framing limit",
            )
        })?;
        envelope.extend_from_slice(&value_len.to_be_bytes());
        envelope.extend_from_slice(value);
    }
    Ok(hmac_sha256_hex(secret.as_bytes(), &envelope))
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
            // Once a sweep has committed an in-flight webhook outbox row, its
            // network future and outcome checkpoint must not be dropped by
            // cooperative shutdown. `process_pending` observes cancellation at
            // safe row boundaries; an active HTTP operation is bounded by the
            // declared reqwest timeout and is durably finalized first.
            let sweep_result = process_pending(&db, now_unix_ms, &shutdown).await;
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
                let preserve_tags = match retained_tier0_toast_tags(&db) {
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
                // Once the serialized COM worker accepts cleanup, this future
                // is an ownership boundary: await the physical mutations and
                // durable audit even if shutdown arrives. Dropping the future
                // cannot cancel the already-enqueued WinRT command.
                let report = remove_orphaned_escalation_toasts(preserve_tags).await;
                loop {
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
                            break;
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::error!(
                                code = "ESCALATION_ORPHAN_TOAST_CLEANUP_AUDIT_RETRY",
                                detail = %error.message,
                                "owned Action Center cleanup completed but its durable audit write/readback failed; retaining ownership and retrying"
                            );
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                if shutdown.is_cancelled() {
                    tracing::info!(
                        code = "ESCALATION_WORKER_STOPPED",
                        stage = "after_owned_orphan_cleanup_audit",
                        "stopping escalation worker only after owned toast cleanup and audit completed"
                    );
                    break;
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
    /// Ordered off-machine egress ladder of `synapse_receipt_v1` receivers.
    /// Replaces the existing list. Empty list ⇒ Tier 0 only (no outbound
    /// calls). Omit to leave webhooks unchanged.
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
    pub channel_id: String,
    pub name: String,
    /// Scheme, host, and explicit port only. URL userinfo, path, query, and
    /// fragment may carry credentials and are never echoed.
    pub endpoint_origin: String,
    pub endpoint_fingerprint_sha256: String,
    pub idempotency_contract: WebhookIdempotencyContract,
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
                    channel_id: channel.channel_id.clone(),
                    name: channel.name.clone(),
                    endpoint_origin: webhook_url_origin(&channel.url),
                    endpoint_fingerprint_sha256: webhook_public_endpoint_fingerprint(channel),
                    idempotency_contract: channel.idempotency_contract,
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
        let mut candidate = EscalationPolicy::default();
        candidate.webhooks.clone_from(webhooks);
        candidate.receiver_generations = candidate
            .webhooks
            .iter()
            .map(|channel| (channel.channel_id.clone(), new_receiver_generation()))
            .collect();
        validate_policy(&candidate, false, error_codes::TOOL_PARAMS_INVALID)?;
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
    if params.quiet_hours.is_some() && !params.set_quiet_hours {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "quiet_hours was supplied but set_quiet_hours=false; set it true to apply or clear the field",
        ));
    }
    Ok(())
}

#[tool_router(router = escalation_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Configure the AFK escalation engine: operator-supplied off-machine webhook egress (Tier 1), the severity threshold, no-ack ladder windows, and quiet hours. Every endpoint must implement synapse_receipt_v1: OPTIONS proves durable idempotency for the delivery ID and body digest; each POST response binds those values and declares committed or not_committed. Synapse refuses to POST to an unproven endpoint and never retries a contradictory receipt. A compliant gateway can fan out to ntfy, Pushover, Telegram/Discord, or phone services behind its durable deduplication boundary. With no webhooks configured the engine is Tier-0-only and makes zero outbound calls. Secrets are stored but never echoed."
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
        let previous_policy = revisioned_policy.policy;
        let mut policy = previous_policy.clone();
        if let Some(webhooks) = params.webhooks {
            let mut generations = BTreeMap::new();
            for channel in &webhooks {
                let preserved = previous_policy
                    .webhooks
                    .iter()
                    .find(|previous| previous.channel_id == channel.channel_id)
                    .filter(|previous| *previous == channel)
                    .and_then(|previous| {
                        previous_policy
                            .receiver_generations
                            .get(&previous.channel_id)
                    })
                    .cloned();
                generations.insert(
                    channel.channel_id.clone(),
                    preserved.unwrap_or_else(new_receiver_generation),
                );
            }
            policy.webhooks = webhooks;
            policy.receiver_generations = generations;
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
        description = "Read the current AFK escalation policy: configured synapse_receipt_v1 off-machine receivers (secrets redacted to a boolean), enforced durable receipt contract, severity threshold, no-ack ladder windows, TTLs, and quiet hours. Reports tier0_only=true when no egress is configured."
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
        let limit = params.limit.unwrap_or(50).min(500) as usize;
        let total_open = count_open_index_rows(&db)?;
        let filtered = list_recent_items(&db, params.status, params.anchor.as_deref(), limit)?;
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
        )?;
        Ok(Json(EscalationAckResponse {
            escalation: outcome.escalation,
            newly_acked: outcome.newly_acked,
        }))
    }
}

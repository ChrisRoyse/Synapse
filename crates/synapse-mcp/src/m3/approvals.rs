//! Durable suggestion/approval queue (#867).
//!
//! Queue truth lives in `CF_KV`, not in daemon memory:
//! - `approval/v1/item/{approval_id}` stores the current item state.
//! - `approval/v1/audit/{approval_id}/{timestamp_ns}-{event_id}` stores every
//!   state transition and timeout materialization.
//!
//! Reads materialize expired pending/snoozed items before returning rows, so
//! timeout-default semantics survive daemon restarts without a background task.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
};

use chrono::Utc;
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synapse_core::{SCHEMA_VERSION, error_codes};
use synapse_storage::{Db, cf, decode_json, encode_json};
use uuid::Uuid;

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

const ITEM_PREFIX: &str = "approval/v1/item/";
const AUDIT_PREFIX: &str = "approval/v1/audit/";
const ACTIVATION_PREFIX: &str = "approval/v1/activation/";
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;
const MAX_SCAN_ROWS: usize = 20_000;
const SCAN_CHUNK_ROWS: usize = 4_096;
const TERMINAL_ITEM_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const TERMINAL_ITEM_RETAIN_ROWS: usize = 5_000;
const DELETE_BATCH_ROWS: usize = 512;
const MAX_TITLE_CHARS: usize = 160;
const MAX_BODY_CHARS: usize = 4_000;
const MAX_PAYLOAD_JSON_BYTES: usize = 64 * 1024;
const MAX_DEDUPE_KEY_CHARS: usize = 256;
const MAX_NOTE_CHARS: usize = 2_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const DEFAULT_SNOOZE_MS: u64 = 15 * 60 * 1_000;
const MAX_SNOOZE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const ACTIVATION_TOKEN_PREFIX: &str = "act1-";
const APPROVAL_PROTOCOL_SCHEME: &str = "synapse-approval";
const TIMEOUT_DECIDER_SESSION: &str = "timeout";

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Suggestion,
    AgentEscalation,
    ArmedRunReview,
    /// A spawned agent paused mid-task and asked the human to allow or deny a
    /// specific tool call before it can continue (#927). Created by the
    /// `approval_gate` permission-prompt tool; the deciding human's verdict is
    /// returned to the still-blocked agent as the gate's `{behavior}` result.
    AgentPermission,
    /// A spawned agent paused to ask the operator a clarifying question / needs
    /// input before it can continue (#1028). The operator's `respond` text is
    /// delivered back to the agent as the next turn; no tool is executed.
    AgentQuestion,
}

impl ApprovalKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Suggestion => "suggestion",
            Self::AgentEscalation => "agent_escalation",
            Self::ArmedRunReview => "armed_run_review",
            Self::AgentPermission => "agent_permission",
            Self::AgentQuestion => "agent_question",
        }
    }
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Accepted,
    Declined,
    Snoozed,
    Ignored,
}

impl ApprovalStatus {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Accepted | Self::Declined | Self::Ignored)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::Snoozed => "snoozed",
            Self::Ignored => "ignored",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Accept,
    Decline,
    Snooze,
}

impl ApprovalDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Snooze => "snooze",
        }
    }

    fn from_activation_text(value: &str) -> Option<Self> {
        match value {
            "accept" => Some(Self::Accept),
            "decline" => Some(Self::Decline),
            "snooze" => Some(Self::Snooze),
            _ => None,
        }
    }
}

/// Per-item affordance flags driving which decision controls the operator
/// surface offers, mirroring the Agent-Inbox `HumanInterruptConfig`
/// (allow_accept / allow_edit / allow_respond / allow_ignore). Stored on the
/// item at request time and never widened by a decision. (#1030)
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAllow {
    /// Approve and run the proposed action unchanged.
    pub accept: bool,
    /// Approve after replacing the proposed tool input with operator-edited
    /// args (full replacement, not a merge — per AG-UI / Agent-Inbox).
    pub edit: bool,
    /// Answer the agent's question; the operator's text becomes the tool/turn
    /// result and the underlying tool is NOT executed.
    pub respond: bool,
    /// Skip the action and let the agent try something else.
    pub ignore: bool,
}

impl Default for ApprovalAllow {
    fn default() -> Self {
        // Conservative default: a plain one-tap accept/deny item, matching the
        // pre-#1030 binary behaviour for rows that predate the `allow` field.
        Self {
            accept: true,
            edit: false,
            respond: false,
            ignore: true,
        }
    }
}

impl ApprovalAllow {
    /// Default affordances for a freshly-requested item of the given kind when
    /// the requester does not specify them. Agent-facing items (tool-permission
    /// pauses, armed runs) allow editing the proposed args; questions allow a
    /// textual response. The taxonomy stays in one place so the dashboard,
    /// harness, and Codex bridge agree.
    #[must_use]
    pub const fn for_kind(kind: ApprovalKind) -> Self {
        match kind {
            ApprovalKind::AgentPermission | ApprovalKind::ArmedRunReview => Self {
                accept: true,
                edit: true,
                respond: false,
                ignore: true,
            },
            ApprovalKind::AgentQuestion => Self {
                accept: false,
                edit: false,
                respond: true,
                ignore: true,
            },
            ApprovalKind::Suggestion | ApprovalKind::AgentEscalation => Self {
                accept: true,
                edit: false,
                respond: false,
                ignore: true,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTimeoutDecision {
    #[default]
    Ignored,
    Declined,
}

impl ApprovalTimeoutDecision {
    const fn status(self) -> ApprovalStatus {
        match self {
            Self::Ignored => ApprovalStatus::Ignored,
            Self::Declined => ApprovalStatus::Declined,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::Declined => "declined",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequestParams {
    pub kind: ApprovalKind,
    pub title: String,
    pub body: String,
    /// Optional JSON payload encoded as a string. This avoids open
    /// `serde_json::Value` input schemas, which strict MCP clients reject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// Optional timeout. Expired items materialize to `timeout_decision` on
    /// the next `approval_list` / `approval_decide` read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Defaults to ignored. `accepted` is intentionally not representable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_decision: Option<ApprovalTimeoutDecision>,
    /// Whether the requested work is destructive. Stored for UI policy; timeout
    /// still cannot accept.
    #[serde(default)]
    pub destructive: bool,
    /// Attempt to create a Windows toast notification for the queued item.
    #[serde(default = "default_notify")]
    pub notify: bool,
    #[serde(default)]
    pub suppress_popup: bool,
    /// Affordances the operator surface should offer for this item (#1030).
    /// Defaults to [`ApprovalAllow::for_kind`] when omitted, so existing callers
    /// (escalation engine, suggestions) keep their accept/deny behaviour and
    /// agent-permission / agent-question requesters opt into edit / respond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<ApprovalAllow>,
}

const fn default_notify() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statuses: Option<Vec<ApprovalStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<ApprovalKind>>,
    /// Include terminal accepted/declined/ignored rows. Defaults false.
    #[serde(default)]
    pub include_terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Hex-encoded item key from a previous response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl Default for ApprovalListParams {
    fn default() -> Self {
        Self {
            statuses: None,
            kinds: None,
            include_terminal: false,
            limit: Some(DEFAULT_LIMIT),
            cursor: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecideParams {
    pub approval_id: String,
    pub decision: ApprovalDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Required for `decision=snooze`; defaults to 15 minutes when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_ms: Option<u64>,
    /// Approve-with-edits (#1030): a full-replacement JSON object (string-encoded
    /// to keep the input schema closed) for the agent's tool input. Honored only
    /// with `decision=accept` on an item whose `allow.edit` is set. Replaces, not
    /// merges, the proposed args.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_args: Option<String>,
    /// Respond (#1030): the operator's textual answer to a needs-input /
    /// `agent_question` item. Honored only with `decision=accept` on an item
    /// whose `allow.respond` is set. Delivered to the agent as the tool/turn
    /// result; the underlying tool is NOT executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalToastState {
    pub requested: bool,
    pub suppress_popup: bool,
    pub actionable_buttons: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_handler_registered: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_setting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_in_history: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalActivationRecord {
    pub schema_version: u32,
    pub activation_id: String,
    pub approval_id: String,
    pub token_sha256: String,
    pub created_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_by_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_decision: Option<ApprovalDecision>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalActivationLinks {
    pub activation_id: String,
    pub accept_uri: String,
    pub decline_uri: String,
    pub snooze_uri: String,
    pub activation_row: ApprovalRowEvidence,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalActivationParams {
    pub bind: String,
    pub approval_id: String,
    pub activation_id: String,
    pub token: String,
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalActivationDecisionResponse {
    pub activation_id: String,
    pub activation_row: ApprovalRowEvidence,
    pub decision: ApprovalDecideResponse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalToastDelivery {
    pub requested: bool,
    pub suppress_popup: bool,
    pub actionable_buttons: bool,
    pub activation_id: Option<String>,
    pub protocol_handler_registered: Option<bool>,
    pub unavailable_reason: Option<String>,
    pub notify_tag: Option<String>,
    pub notify_group: Option<String>,
    pub notification_setting: Option<String>,
    pub verified_in_history: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalItemRecord {
    pub schema_version: u32,
    pub approval_id: String,
    pub kind: ApprovalKind,
    pub status: ApprovalStatus,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    pub destructive: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    pub timeout_decision: ApprovalTimeoutDecision,
    pub requested_by_session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_note: Option<String>,
    /// Affordances the operator surface may offer for this item (#1030). Rows
    /// written before #1030 deserialize to [`ApprovalAllow::default`].
    #[serde(default)]
    pub allow: ApprovalAllow,
    /// Operator-edited, full-replacement tool input recorded when the item was
    /// approved-with-edits (#1030). The blocked agent runs THIS, not its
    /// proposed args. JSON object, string-encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_args_json: Option<String>,
    /// Operator's textual answer recorded when a needs-input item was resolved
    /// via `respond` (#1030). Delivered to the agent instead of running a tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_response: Option<String>,
    pub toast: ApprovalToastState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAuditRecord {
    pub schema_version: u32,
    pub approval_id: String,
    pub event_id: String,
    pub event: String,
    pub at_unix_ms: u64,
    pub by_session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_status: Option<ApprovalStatus>,
    pub after_status: ApprovalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRowEvidence {
    pub cf_name: String,
    pub key: String,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalQueueItem {
    pub item: ApprovalItemRecord,
    pub item_row: ApprovalRowEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalMaterializedTimeout {
    pub item: ApprovalItemRecord,
    pub item_row: ApprovalRowEvidence,
    pub audit_row: ApprovalRowEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequestResponse {
    pub deduped: bool,
    pub item: ApprovalItemRecord,
    pub item_row: ApprovalRowEvidence,
    pub audit_row: ApprovalRowEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toast_audit_row: Option<ApprovalRowEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListResponse {
    pub now_unix_ms: u64,
    pub items: Vec<ApprovalQueueItem>,
    pub materialized_timeouts: Vec<ApprovalMaterializedTimeout>,
    pub scanned_rows: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecideResponse {
    pub approval_id: String,
    pub before_status: ApprovalStatus,
    pub after_status: ApprovalStatus,
    pub item: ApprovalItemRecord,
    pub item_row: ApprovalRowEvidence,
    pub audit_row: ApprovalRowEvidence,
}

#[must_use]
pub const fn approval_request() -> M3ToolStub {
    M3ToolStub::new("approval_request")
}

#[must_use]
pub const fn approval_list() -> M3ToolStub {
    M3ToolStub::new("approval_list")
}

#[must_use]
pub const fn approval_decide() -> M3ToolStub {
    M3ToolStub::new("approval_decide")
}

#[must_use]
pub fn required_permissions_request(_params: &ApprovalRequestParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_list(_params: &ApprovalListParams) -> RequiredPermissions {
    // Listing materializes timeout-default transitions, so it writes audit/item
    // rows when physical time has passed.
    required([Permission::ReadStorage, Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_decide(_params: &ApprovalDecideParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

pub fn request_approval(
    db: &Arc<Db>,
    params: &ApprovalRequestParams,
    by_session: &str,
) -> Result<ApprovalRequestResponse, ErrorData> {
    validate_request(params)?;
    let now = now_unix_ms();
    if let Some(existing) = find_pending_dedupe(db, params.dedupe_key.as_deref(), now)? {
        let audit = write_audit(
            db,
            &existing.item.approval_id,
            "dedupe_returned",
            now,
            by_session,
            Some(existing.item.status),
            existing.item.status,
            Some("existing pending approval returned for dedupe_key".to_owned()),
        )?;
        return Ok(ApprovalRequestResponse {
            deduped: true,
            item: existing.item,
            item_row: existing.item_row,
            audit_row: audit,
            toast_audit_row: None,
        });
    }

    let approval_id = format!("apr1-{}", Uuid::now_v7().simple());
    let expires_at_unix_ms = params
        .timeout_ms
        .map(|timeout_ms| now.saturating_add(timeout_ms));
    let item = ApprovalItemRecord {
        schema_version: SCHEMA_VERSION,
        approval_id: approval_id.clone(),
        kind: params.kind,
        status: ApprovalStatus::Pending,
        title: params.title.trim().to_owned(),
        body: params.body.trim().to_owned(),
        payload_json: params.payload_json.clone(),
        dedupe_key: normalized_optional(params.dedupe_key.as_deref()),
        destructive: params.destructive,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        expires_at_unix_ms,
        timeout_decision: params.timeout_decision.unwrap_or_default(),
        requested_by_session: by_session.to_owned(),
        decided_by_session: None,
        decided_at_unix_ms: None,
        decision_note: None,
        allow: params
            .allow
            .unwrap_or_else(|| ApprovalAllow::for_kind(params.kind)),
        edited_args_json: None,
        operator_response: None,
        toast: ApprovalToastState {
            requested: params.notify,
            suppress_popup: params.suppress_popup,
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
    let (item_row, audit_row) = write_item_and_audit(
        db,
        &item,
        &approval_id,
        "requested",
        now,
        by_session,
        None,
        ApprovalStatus::Pending,
        None,
    )?;
    Ok(ApprovalRequestResponse {
        deduped: false,
        item,
        item_row,
        audit_row,
        toast_audit_row: None,
    })
}

pub fn update_approval_toast_state(
    db: &Arc<Db>,
    approval_id: &str,
    delivery: ApprovalToastDelivery,
    by_session: &str,
) -> Result<(ApprovalItemRecord, ApprovalRowEvidence, ApprovalRowEvidence), ErrorData> {
    validate_approval_id(approval_id)?;
    let now = now_unix_ms();
    let key = item_key(approval_id);
    let (mut item, _before_row) = read_item_by_key(db, &key)?.ok_or_else(|| {
        invalid(format!(
            "approval toast update approval_id {approval_id:?} does not exist",
        ))
    })?;
    item.toast = ApprovalToastState {
        requested: delivery.requested,
        suppress_popup: delivery.suppress_popup,
        actionable_buttons: delivery.actionable_buttons,
        activation_id: delivery.activation_id,
        protocol_handler_registered: delivery.protocol_handler_registered,
        unavailable_reason: delivery.unavailable_reason,
        notify_tag: delivery.notify_tag,
        notify_group: delivery.notify_group,
        notification_setting: delivery.notification_setting,
        verified_in_history: delivery.verified_in_history,
    };
    item.updated_at_unix_ms = now;
    let (item_row, audit_row) = write_item_and_audit(
        db,
        &item,
        approval_id,
        "toast_updated",
        now,
        by_session,
        Some(item.status),
        item.status,
        Some(format!(
            "toast requested={} actionable_buttons={}",
            item.toast.requested, item.toast.actionable_buttons
        )),
    )?;
    Ok((item, item_row, audit_row))
}

pub fn prepare_activation_links(
    db: &Arc<Db>,
    approval_id: &str,
    bind_addr: &str,
) -> Result<ApprovalActivationLinks, ErrorData> {
    validate_approval_id(approval_id)?;
    validate_bind_addr(bind_addr)?;
    let activation_id = format!("actv1-{}", Uuid::now_v7().simple());
    let token = format!(
        "{ACTIVATION_TOKEN_PREFIX}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    let record = ApprovalActivationRecord {
        schema_version: SCHEMA_VERSION,
        activation_id: activation_id.clone(),
        approval_id: approval_id.to_owned(),
        token_sha256: activation_token_sha256(&token),
        created_at_unix_ms: now_unix_ms(),
        used_at_unix_ms: None,
        used_by_session: None,
        used_decision: None,
    };
    let activation_row = write_activation(db, &record)?;
    let accept_uri = activation_uri(
        bind_addr,
        approval_id,
        &activation_id,
        &token,
        "accept",
        None,
    );
    let decline_uri = activation_uri(
        bind_addr,
        approval_id,
        &activation_id,
        &token,
        "decline",
        None,
    );
    let snooze_uri = activation_uri(
        bind_addr,
        approval_id,
        &activation_id,
        &token,
        "snooze",
        Some(DEFAULT_SNOOZE_MS),
    );
    Ok(ApprovalActivationLinks {
        activation_id,
        accept_uri,
        decline_uri,
        snooze_uri,
        activation_row,
    })
}

pub fn parse_activation_uri(uri: &str) -> Result<ApprovalActivationParams, ErrorData> {
    let query = uri
        .strip_prefix(&format!("{APPROVAL_PROTOCOL_SCHEME}://decide?"))
        .ok_or_else(|| {
            invalid(format!(
                "expected {APPROVAL_PROTOCOL_SCHEME}://decide? activation URI"
            ))
        })?;
    let fields = parse_query(query)?;
    let params = ApprovalActivationParams {
        bind: required_query_field(&fields, "bind")?,
        approval_id: required_query_field(&fields, "approval_id")?,
        activation_id: required_query_field(&fields, "activation_id")?,
        token: required_query_field(&fields, "token")?,
        decision: required_query_field(&fields, "decision")?,
        snooze_ms: match fields.get("snooze_ms").map(String::as_str) {
            Some(value) if !value.is_empty() => Some(value.parse::<u64>().map_err(|error| {
                invalid(format!(
                    "activation URI snooze_ms must be an integer: {error}"
                ))
            })?),
            _ => None,
        },
    };
    validate_activation_params(&params)?;
    Ok(params)
}

pub fn decide_approval_from_activation(
    db: &Arc<Db>,
    params: &ApprovalActivationParams,
    by_session: &str,
) -> Result<ApprovalActivationDecisionResponse, ErrorData> {
    validate_activation_params(params)?;
    let decision = ApprovalDecision::from_activation_text(params.decision.as_str())
        .ok_or_else(|| invalid("activation decision must be accept, decline, or snooze"))?;
    let key = activation_key(&params.approval_id, &params.activation_id);
    let (mut activation, _before_row) = read_activation_by_key(db, &key)?.ok_or_else(|| {
        invalid(format!(
            "activation_id {:?} for approval_id {:?} does not exist",
            params.activation_id, params.approval_id
        ))
    })?;
    if activation.used_at_unix_ms.is_some() {
        return Err(invalid(format!(
            "activation_id {:?} was already used",
            params.activation_id
        )));
    }
    let expected_hash = activation_token_sha256(&params.token);
    if activation.token_sha256 != expected_hash {
        return Err(invalid(
            "activation token did not match the stored token hash",
        ));
    }
    let decision_response = decide_approval(
        db,
        &ApprovalDecideParams {
            approval_id: params.approval_id.clone(),
            decision,
            note: Some(format!(
                "resolved through toast activation {}",
                params.activation_id
            )),
            snooze_ms: params.snooze_ms,
            // Toast activations are one-tap accept/decline/snooze — no inline
            // edit/respond surface (#1030).
            edited_args: None,
            response: None,
        },
        by_session,
    )?;
    activation.used_at_unix_ms = Some(now_unix_ms());
    activation.used_by_session = Some(by_session.to_owned());
    activation.used_decision = Some(decision);
    let activation_row = write_activation(db, &activation)?;
    Ok(ApprovalActivationDecisionResponse {
        activation_id: activation.activation_id,
        activation_row,
        decision: decision_response,
    })
}

pub fn list_approvals(
    db: &Arc<Db>,
    params: &ApprovalListParams,
) -> Result<ApprovalListResponse, ErrorData> {
    validate_list(params)?;
    let now = now_unix_ms();
    let materialized_timeouts = materialize_timeouts(db, now)?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let limit_usize = usize::try_from(limit).unwrap_or(MAX_LIMIT as usize);
    let status_filter = params
        .statuses
        .as_deref()
        .map(|values| values.iter().copied().collect::<BTreeSet<_>>());
    let kind_filter = params
        .kinds
        .as_deref()
        .map(|values| values.iter().copied().collect::<BTreeSet<_>>());
    let mut start_key = match params
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(cursor) => key_after(&hex_decode(cursor).ok_or_else(|| {
            invalid("approval_list cursor must be a hex item key from a previous response")
        })?),
        None => ITEM_PREFIX.as_bytes().to_vec(),
    };
    let mut items = Vec::new();
    let mut scanned_rows = 0_u64;
    let mut next_cursor = None;
    'scan: loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_KV, &start_key, SCAN_CHUNK_ROWS)
            .map_err(storage_error)?;
        if rows.is_empty() {
            break;
        }
        let mut last_key = None;
        for (key, value) in rows {
            if !key.starts_with(ITEM_PREFIX.as_bytes()) {
                break 'scan;
            }
            scanned_rows = scanned_rows.saturating_add(1);
            let item = decode_item(&key, &value)?;
            last_key = Some(key.clone());
            if !params.include_terminal && item.status.is_terminal() {
                continue;
            }
            if let Some(filter) = &status_filter {
                if !filter.contains(&item.status) {
                    continue;
                }
            }
            if let Some(filter) = &kind_filter {
                if !filter.contains(&item.kind) {
                    continue;
                }
            }
            let row = row_evidence(cf::CF_KV, &key, &value);
            items.push(ApprovalQueueItem {
                item,
                item_row: row,
            });
            if items.len() == limit_usize {
                next_cursor = Some(hex_encode(&key));
                break 'scan;
            }
        }
        if !more {
            break;
        }
        let Some(key) = last_key else {
            break;
        };
        start_key = key_after(&key);
    }
    Ok(ApprovalListResponse {
        now_unix_ms: now,
        items,
        materialized_timeouts,
        scanned_rows,
        next_cursor,
    })
}

pub fn decide_approval(
    db: &Arc<Db>,
    params: &ApprovalDecideParams,
    by_session: &str,
) -> Result<ApprovalDecideResponse, ErrorData> {
    validate_decide(params)?;
    let now = now_unix_ms();
    let _materialized = materialize_timeouts(db, now)?;
    let key = item_key(&params.approval_id);
    let (mut item, _before_row) = read_item_by_key(db, &key)?.ok_or_else(|| {
        invalid(format!(
            "approval_decide approval_id {:?} does not exist",
            params.approval_id
        ))
    })?;
    let before_status = item.status;
    if before_status.is_terminal() {
        return Err(invalid(format!(
            "approval_decide cannot change terminal approval {} from {}",
            item.approval_id,
            before_status.as_str()
        )));
    }
    // Item-aware affordance gating (#1030): a decision may only use an
    // affordance the item actually advertises. Advertise-then-deny would let an
    // operator edit args on an item the agent never agreed to have edited.
    if params.edited_args.is_some() && !item.allow.edit {
        return Err(invalid(format!(
            "approval_decide approval {} does not allow approve-with-edits (allow.edit=false)",
            item.approval_id
        )));
    }
    if params.response.is_some() && !item.allow.respond {
        return Err(invalid(format!(
            "approval_decide approval {} does not allow a respond answer (allow.respond=false)",
            item.approval_id
        )));
    }
    // Respond items resolve via the operator's answer, so accepting one without
    // a response is a no-op that would strand the agent — require it.
    if params.decision == ApprovalDecision::Accept
        && item.kind == ApprovalKind::AgentQuestion
        && params.response.is_none()
    {
        return Err(invalid(format!(
            "approval_decide accepting agent_question {} requires a `response` answer",
            item.approval_id
        )));
    }
    let note = normalized_optional(params.note.as_deref());
    // Reject-requires-note for agent-facing items (#1030): the note is the
    // feedback fed back into the blocked agent's context. A bare denial leaves
    // the agent guessing.
    if params.decision == ApprovalDecision::Decline
        && matches!(
            item.kind,
            ApprovalKind::AgentPermission | ApprovalKind::AgentQuestion
        )
        && note.is_none()
    {
        return Err(invalid(format!(
            "approval_decide declining {} {} requires a `note` explaining why (it is fed back to the agent)",
            item.kind.as_str(),
            item.approval_id
        )));
    }
    let after_status = match params.decision {
        ApprovalDecision::Accept => ApprovalStatus::Accepted,
        ApprovalDecision::Decline => ApprovalStatus::Declined,
        ApprovalDecision::Snooze => ApprovalStatus::Snoozed,
    };
    item.status = after_status;
    item.updated_at_unix_ms = now;
    item.decided_by_session = Some(by_session.to_owned());
    item.decided_at_unix_ms = Some(now);
    item.decision_note = note.clone();
    // Persist the approve-with-edits / respond payloads so the blocked agent
    // (and the audit trail) can read exactly what the operator authorized.
    item.edited_args_json = params.edited_args.clone();
    item.operator_response = normalized_optional(params.response.as_deref());
    item.expires_at_unix_ms = match params.decision {
        ApprovalDecision::Snooze => {
            Some(now.saturating_add(params.snooze_ms.unwrap_or(DEFAULT_SNOOZE_MS)))
        }
        ApprovalDecision::Accept | ApprovalDecision::Decline => None,
    };
    let (item_row, audit_row) = write_item_and_audit(
        db,
        &item,
        &item.approval_id,
        params.decision.as_str(),
        now,
        by_session,
        Some(before_status),
        after_status,
        note,
    )?;
    Ok(ApprovalDecideResponse {
        approval_id: item.approval_id.clone(),
        before_status,
        after_status,
        item,
        item_row,
        audit_row,
    })
}

/// Side-effect-free read of one approval item by id. Returns `None` when no
/// row exists. Unlike [`list_approvals`]/[`decide_approval`] this does NOT
/// materialize timeout transitions — the caller (e.g. the `approval_gate`
/// blocking loop) reads the raw current status as the source of truth and
/// enforces its own deadline.
pub fn get_approval(
    db: &Arc<Db>,
    approval_id: &str,
) -> Result<Option<ApprovalQueueItem>, ErrorData> {
    let key = item_key(approval_id);
    Ok(read_item_by_key(db, &key)?.map(|(item, item_row)| ApprovalQueueItem { item, item_row }))
}

pub fn approval_snapshot(
    db: &Arc<Db>,
    kind: Option<ApprovalKind>,
) -> Result<Vec<ApprovalQueueItem>, ErrorData> {
    let params = ApprovalListParams {
        statuses: Some(vec![ApprovalStatus::Pending, ApprovalStatus::Snoozed]),
        kinds: kind.map(|value| vec![value]),
        include_terminal: false,
        // Sized for a fleet: the Approvals inbox must surface many concurrent
        // agent_permission pauses, not just the most recent handful.
        limit: Some(200),
        cursor: None,
    };
    list_approvals(db, &params).map(|response| response.items)
}

fn validate_request(params: &ApprovalRequestParams) -> Result<(), ErrorData> {
    validate_nonblank(&params.title, "approval_request title", MAX_TITLE_CHARS)?;
    validate_nonblank(&params.body, "approval_request body", MAX_BODY_CHARS)?;
    if let Some(payload_json) = params.payload_json.as_deref() {
        if payload_json.len() > MAX_PAYLOAD_JSON_BYTES {
            return Err(invalid(format!(
                "approval_request payload_json is {} bytes; max {MAX_PAYLOAD_JSON_BYTES}",
                payload_json.len()
            )));
        }
        serde_json::from_str::<serde_json::Value>(payload_json).map_err(|error| {
            invalid(format!(
                "approval_request payload_json must be valid JSON text: {error}"
            ))
        })?;
    }
    if let Some(dedupe_key) = params.dedupe_key.as_deref() {
        validate_optional_nonblank(
            dedupe_key,
            "approval_request dedupe_key",
            MAX_DEDUPE_KEY_CHARS,
        )?;
    }
    if let Some(timeout_ms) = params.timeout_ms {
        validate_duration(
            timeout_ms,
            MIN_TIMEOUT_MS,
            MAX_TIMEOUT_MS,
            "approval_request timeout_ms",
        )?;
    }
    Ok(())
}

fn validate_list(params: &ApprovalListParams) -> Result<(), ErrorData> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(invalid(format!(
            "approval_list limit must be between 1 and {MAX_LIMIT}; got {limit}"
        )));
    }
    if params.statuses.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid("approval_list statuses must not be empty"));
    }
    if params.kinds.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid("approval_list kinds must not be empty"));
    }
    Ok(())
}

fn validate_decide(params: &ApprovalDecideParams) -> Result<(), ErrorData> {
    validate_approval_id(&params.approval_id)?;
    if let Some(note) = params.note.as_deref() {
        validate_optional_nonblank(note, "approval_decide note", MAX_NOTE_CHARS)?;
    }
    if let Some(snooze_ms) = params.snooze_ms {
        validate_duration(
            snooze_ms,
            MIN_TIMEOUT_MS,
            MAX_SNOOZE_MS,
            "approval_decide snooze_ms",
        )?;
    }
    if params.decision != ApprovalDecision::Snooze && params.snooze_ms.is_some() {
        return Err(invalid(
            "approval_decide snooze_ms is valid only when decision=\"snooze\"",
        ));
    }
    // Approve-with-edits / respond are accept-only modifiers (#1030). They make
    // no sense on decline/snooze and must fail loudly rather than be silently
    // dropped.
    if params.decision != ApprovalDecision::Accept {
        if params.edited_args.is_some() {
            return Err(invalid(
                "approval_decide edited_args is valid only when decision=\"accept\"",
            ));
        }
        if params.response.is_some() {
            return Err(invalid(
                "approval_decide response is valid only when decision=\"accept\"",
            ));
        }
    }
    if let Some(edited_args) = params.edited_args.as_deref() {
        if edited_args.len() > MAX_PAYLOAD_JSON_BYTES {
            return Err(invalid(format!(
                "approval_decide edited_args is {} bytes; max {MAX_PAYLOAD_JSON_BYTES}",
                edited_args.len()
            )));
        }
        // Full-replacement tool input must be a JSON OBJECT (a tool's argument
        // map). Reject scalars/arrays/garbage here so a malformed edit never
        // reaches the agent as a dispatched call.
        match serde_json::from_str::<serde_json::Value>(edited_args) {
            Ok(serde_json::Value::Object(_)) => {}
            Ok(_) => {
                return Err(invalid(
                    "approval_decide edited_args must be a JSON object (the tool's argument map)",
                ));
            }
            Err(error) => {
                return Err(invalid(format!(
                    "approval_decide edited_args must be valid JSON object text: {error}"
                )));
            }
        }
    }
    if let Some(response) = params.response.as_deref() {
        validate_nonblank(response, "approval_decide response", MAX_BODY_CHARS)?;
    }
    Ok(())
}

fn validate_activation_params(params: &ApprovalActivationParams) -> Result<(), ErrorData> {
    validate_bind_addr(&params.bind)?;
    validate_approval_id(&params.approval_id)?;
    validate_activation_id(&params.activation_id)?;
    validate_activation_token(&params.token)?;
    if ApprovalDecision::from_activation_text(params.decision.as_str()).is_none() {
        return Err(invalid(
            "activation decision must be accept, decline, or snooze",
        ));
    }
    if let Some(snooze_ms) = params.snooze_ms {
        validate_duration(
            snooze_ms,
            MIN_TIMEOUT_MS,
            MAX_SNOOZE_MS,
            "approval activation snooze_ms",
        )?;
    }
    if params.decision != "snooze" && params.snooze_ms.is_some() {
        return Err(invalid(
            "approval activation snooze_ms is valid only when decision=\"snooze\"",
        ));
    }
    Ok(())
}

fn validate_bind_addr(value: &str) -> Result<(), ErrorData> {
    let addr = value.parse::<SocketAddr>().map_err(|error| {
        invalid(format!(
            "approval activation bind must be host:port: {error}"
        ))
    })?;
    if !addr.ip().is_loopback() {
        return Err(invalid(
            "approval activation bind must be loopback; refusing non-local callback URI",
        ));
    }
    Ok(())
}

fn validate_nonblank(value: &str, field: &str, max_chars: usize) -> Result<(), ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid(format!(
            "{field} must not be empty or whitespace-only"
        )));
    }
    let chars = trimmed.chars().count();
    if chars > max_chars {
        return Err(invalid(format!(
            "{field} is {chars} characters; max {max_chars}"
        )));
    }
    if let Some(control) = trimmed.chars().find(|ch| ch.is_control()) {
        return Err(invalid(format!(
            "{field} contains control character U+{:04X}",
            control as u32
        )));
    }
    Ok(())
}

fn validate_optional_nonblank(value: &str, field: &str, max_chars: usize) -> Result<(), ErrorData> {
    validate_nonblank(value, field, max_chars)
}

fn validate_duration(value: u64, min: u64, max: u64, field: &str) -> Result<(), ErrorData> {
    if value < min || value > max {
        return Err(invalid(format!(
            "{field} must be between {min} and {max} ms; got {value}"
        )));
    }
    Ok(())
}

fn validate_approval_id(value: &str) -> Result<(), ErrorData> {
    let valid = value.len() == "apr1-".len() + 32
        && value.starts_with("apr1-")
        && value["apr1-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(invalid(
            "approval_id must be formatted as apr1- plus 32 hex characters",
        ))
    }
}

fn validate_activation_id(value: &str) -> Result<(), ErrorData> {
    let valid = value.len() == "actv1-".len() + 32
        && value.starts_with("actv1-")
        && value["actv1-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(invalid(
            "activation_id must be formatted as actv1- plus 32 hex characters",
        ))
    }
}

fn validate_activation_token(value: &str) -> Result<(), ErrorData> {
    let hex = value.strip_prefix(ACTIVATION_TOKEN_PREFIX).ok_or_else(|| {
        invalid(format!(
            "activation token must start with {ACTIVATION_TOKEN_PREFIX}"
        ))
    })?;
    if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(invalid(
            "activation token must contain 64 hex characters after the prefix",
        ))
    }
}

#[derive(Clone, Debug)]
struct ApprovalItemRow {
    key: Vec<u8>,
    value: Vec<u8>,
    item: ApprovalItemRecord,
}

fn scan_item_rows(db: &Arc<Db>) -> Result<Vec<ApprovalItemRow>, ErrorData> {
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
            let item = decode_item(&key, &value)?;
            out.push(ApprovalItemRow {
                key: key.clone(),
                value,
                item,
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

fn delete_keys(db: &Arc<Db>, mut keys: Vec<Vec<u8>>, context: &str) -> Result<usize, ErrorData> {
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
    db: &Arc<Db>,
    now: u64,
    rows: &[ApprovalItemRow],
) -> Result<usize, ErrorData> {
    let mut terminal = rows
        .iter()
        .filter(|row| row.item.status.is_terminal())
        .map(|row| (row.item.updated_at_unix_ms, row.key.clone()))
        .collect::<Vec<_>>();
    terminal.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut delete = terminal
        .iter()
        .filter(|(updated_at, _key)| updated_at.saturating_add(TERMINAL_ITEM_RETENTION_MS) <= now)
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

    let deleted = delete_keys(db, delete, "approval terminal retention")?;
    if deleted > 0 {
        tracing::info!(
            code = "APPROVAL_ITEM_RETENTION_PRUNED",
            scanned_rows = rows.len(),
            terminal_rows = terminal.len(),
            deleted_rows = deleted,
            retain_terminal_rows = TERMINAL_ITEM_RETAIN_ROWS,
            retention_ms = TERMINAL_ITEM_RETENTION_MS,
            "readback=CF_KV approval terminal item rows pruned"
        );
    } else if rows.len() > MAX_SCAN_ROWS {
        tracing::warn!(
            code = "APPROVAL_ITEM_QUEUE_LARGE",
            scanned_rows = rows.len(),
            terminal_rows = terminal.len(),
            retain_terminal_rows = TERMINAL_ITEM_RETAIN_ROWS,
            "approval item scan exceeded historical hard limit but continued"
        );
    }
    Ok(deleted)
}

fn prune_terminal_items(db: &Arc<Db>, now: u64) -> Result<usize, ErrorData> {
    let rows = scan_item_rows(db)?;
    prune_terminal_item_rows(db, now, &rows)
}

fn find_pending_dedupe(
    db: &Arc<Db>,
    dedupe_key: Option<&str>,
    now: u64,
) -> Result<Option<ApprovalQueueItem>, ErrorData> {
    let Some(dedupe_key) = normalized_optional(dedupe_key) else {
        return Ok(None);
    };
    materialize_timeouts(db, now)?;
    let rows = scan_item_rows(db)?;
    if rows.len() > MAX_SCAN_ROWS {
        tracing::warn!(
            code = "APPROVAL_DEDUPE_QUEUE_LARGE",
            scanned_rows = rows.len(),
            "approval_request dedupe scan exceeded historical hard limit but continued"
        );
    }
    for row in rows {
        if matches!(
            row.item.status,
            ApprovalStatus::Pending | ApprovalStatus::Snoozed
        ) && row.item.dedupe_key.as_deref() == Some(dedupe_key.as_str())
        {
            return Ok(Some(ApprovalQueueItem {
                item: row.item,
                item_row: row_evidence(cf::CF_KV, &row.key, &row.value),
            }));
        }
    }
    Ok(None)
}

fn materialize_timeouts(
    db: &Arc<Db>,
    now: u64,
) -> Result<Vec<ApprovalMaterializedTimeout>, ErrorData> {
    let rows = scan_item_rows(db)?;
    let mut materialized = Vec::new();
    for row in &rows {
        let mut item = row.item.clone();
        if item.status.is_terminal() {
            continue;
        }
        let Some(deadline) = item.expires_at_unix_ms else {
            continue;
        };
        if now < deadline {
            continue;
        }
        let before = item.status;
        item.status = item.timeout_decision.status();
        item.updated_at_unix_ms = now;
        item.decided_at_unix_ms = Some(now);
        item.decided_by_session = Some(TIMEOUT_DECIDER_SESSION.to_owned());
        item.decision_note = Some(format!(
            "timeout default materialized as {}",
            item.timeout_decision.as_str()
        ));
        item.expires_at_unix_ms = None;
        let (row, audit_row) = write_item_and_audit(
            db,
            &item,
            &item.approval_id,
            "timeout_default",
            now,
            TIMEOUT_DECIDER_SESSION,
            Some(before),
            item.status,
            item.decision_note.clone(),
        )?;
        materialized.push(ApprovalMaterializedTimeout {
            item,
            item_row: row,
            audit_row,
        });
    }
    prune_terminal_item_rows(db, now, &rows)?;
    if !materialized.is_empty() {
        prune_terminal_items(db, now)?;
    }
    Ok(materialized)
}

fn write_item_and_audit(
    db: &Arc<Db>,
    item: &ApprovalItemRecord,
    approval_id: &str,
    event: &str,
    at_unix_ms: u64,
    by_session: &str,
    before_status: Option<ApprovalStatus>,
    after_status: ApprovalStatus,
    note: Option<String>,
) -> Result<(ApprovalRowEvidence, ApprovalRowEvidence), ErrorData> {
    let (item_key, item_value) = item_kv(item)?;
    let (audit_key, audit_value) = audit_kv(
        approval_id,
        event,
        at_unix_ms,
        by_session,
        before_status,
        after_status,
        note,
    )?;
    db.put_batch_pressure_bypass(
        cf::CF_KV,
        [
            (item_key.clone(), item_value),
            (audit_key.clone(), audit_value),
        ],
    )
    .map_err(storage_error)?;
    let item_row = readback_row(db, &item_key, "approval item+audit item write")?;
    let audit_row = readback_row(db, &audit_key, "approval item+audit audit write")?;
    Ok((item_row, audit_row))
}

fn write_audit(
    db: &Arc<Db>,
    approval_id: &str,
    event: &str,
    at_unix_ms: u64,
    by_session: &str,
    before_status: Option<ApprovalStatus>,
    after_status: ApprovalStatus,
    note: Option<String>,
) -> Result<ApprovalRowEvidence, ErrorData> {
    let (key, value) = audit_kv(
        approval_id,
        event,
        at_unix_ms,
        by_session,
        before_status,
        after_status,
        note,
    )?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(key.clone(), value)])
        .map_err(storage_error)?;
    readback_row(db, &key, "approval audit write")
}

fn item_kv(item: &ApprovalItemRecord) -> Result<(Vec<u8>, Vec<u8>), ErrorData> {
    let key = item_key(&item.approval_id);
    let value = encode_json(item).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "approval item encode failed for {}: {error}",
                item.approval_id
            ),
        )
    })?;
    Ok((key, value))
}

fn audit_kv(
    approval_id: &str,
    event: &str,
    at_unix_ms: u64,
    by_session: &str,
    before_status: Option<ApprovalStatus>,
    after_status: ApprovalStatus,
    note: Option<String>,
) -> Result<(Vec<u8>, Vec<u8>), ErrorData> {
    let audit = ApprovalAuditRecord {
        schema_version: SCHEMA_VERSION,
        approval_id: approval_id.to_owned(),
        event_id: Uuid::now_v7().to_string(),
        event: event.to_owned(),
        at_unix_ms,
        by_session: by_session.to_owned(),
        before_status,
        after_status,
        note,
    };
    let key = audit_key(approval_id, at_unix_ms, &audit.event_id);
    let value = encode_json(&audit).map_err(|error| {
        mcp_error(
            error.code(),
            format!("approval audit encode failed for {approval_id}/{event}: {error}"),
        )
    })?;
    Ok((key, value))
}

fn write_activation(
    db: &Arc<Db>,
    activation: &ApprovalActivationRecord,
) -> Result<ApprovalRowEvidence, ErrorData> {
    let key = activation_key(&activation.approval_id, &activation.activation_id);
    let value = encode_json(activation).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "approval activation encode failed for {}/{}: {error}",
                activation.approval_id, activation.activation_id
            ),
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(key.clone(), value)])
        .map_err(storage_error)?;
    readback_row(db, &key, "approval activation write")
}

fn read_item_by_key(
    db: &Arc<Db>,
    key: &[u8],
) -> Result<Option<(ApprovalItemRecord, ApprovalRowEvidence)>, ErrorData> {
    let rows = db.scan_cf_prefix(cf::CF_KV, key).map_err(storage_error)?;
    for (row_key, value) in rows {
        if row_key == key {
            let item = decode_item(&row_key, &value)?;
            return Ok(Some((item, row_evidence(cf::CF_KV, &row_key, &value))));
        }
    }
    Ok(None)
}

fn read_activation_by_key(
    db: &Arc<Db>,
    key: &[u8],
) -> Result<Option<(ApprovalActivationRecord, ApprovalRowEvidence)>, ErrorData> {
    let rows = db.scan_cf_prefix(cf::CF_KV, key).map_err(storage_error)?;
    for (row_key, value) in rows {
        if row_key == key {
            let activation = decode_json::<ApprovalActivationRecord>(&value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "approval activation decode failed for key_hex={}: {error}",
                        hex_encode(&row_key)
                    ),
                )
            })?;
            return Ok(Some((
                activation,
                row_evidence(cf::CF_KV, &row_key, &value),
            )));
        }
    }
    Ok(None)
}

fn readback_row(db: &Arc<Db>, key: &[u8], context: &str) -> Result<ApprovalRowEvidence, ErrorData> {
    let rows = db.scan_cf_prefix(cf::CF_KV, key).map_err(storage_error)?;
    for (row_key, value) in rows {
        if row_key == key {
            return Ok(row_evidence(cf::CF_KV, &row_key, &value));
        }
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "{context} had no physical readback row: key_hex={}",
            hex_encode(key)
        ),
    ))
}

fn decode_item(key: &[u8], value: &[u8]) -> Result<ApprovalItemRecord, ErrorData> {
    decode_json::<ApprovalItemRecord>(value).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "approval item decode failed for key_hex={}: {error}",
                hex_encode(key)
            ),
        )
    })
}

fn item_key(approval_id: &str) -> Vec<u8> {
    format!("{ITEM_PREFIX}{approval_id}").into_bytes()
}

fn audit_key(approval_id: &str, at_unix_ms: u64, event_id: &str) -> Vec<u8> {
    format!("{AUDIT_PREFIX}{approval_id}/{at_unix_ms:020}-{event_id}").into_bytes()
}

fn activation_key(approval_id: &str, activation_id: &str) -> Vec<u8> {
    format!("{ACTIVATION_PREFIX}{approval_id}/{activation_id}").into_bytes()
}

fn activation_uri(
    bind_addr: &str,
    approval_id: &str,
    activation_id: &str,
    token: &str,
    decision: &str,
    snooze_ms: Option<u64>,
) -> String {
    let mut uri = format!(
        "{APPROVAL_PROTOCOL_SCHEME}://decide?bind={}&approval_id={}&activation_id={}&token={}&decision={}",
        url_encode(bind_addr),
        url_encode(approval_id),
        url_encode(activation_id),
        url_encode(token),
        url_encode(decision),
    );
    if let Some(snooze_ms) = snooze_ms {
        uri.push_str("&snooze_ms=");
        uri.push_str(&snooze_ms.to_string());
    }
    uri
}

fn row_evidence(cf_name: &str, key: &[u8], value: &[u8]) -> ApprovalRowEvidence {
    ApprovalRowEvidence {
        cf_name: cf_name.to_owned(),
        key: String::from_utf8_lossy(key).into_owned(),
        key_hex: hex_encode(key),
        value_len_bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
        value_sha256: sha256_hex(value),
    }
}

fn normalized_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn now_unix_ms() -> u64 {
    Utc::now().timestamp_millis().try_into().unwrap_or_default()
}

fn storage_error(error: synapse_storage::StorageError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
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

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

fn parse_query(raw: &str) -> Result<BTreeMap<String, String>, ErrorData> {
    let mut fields = BTreeMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| invalid(format!("activation URI query pair {pair:?} is missing '='")))?;
        let key = url_decode(key)?;
        let value = url_decode(value)?;
        if fields.insert(key.clone(), value).is_some() {
            return Err(invalid(format!(
                "activation URI contains duplicate query field {key:?}"
            )));
        }
    }
    Ok(fields)
}

fn required_query_field(
    fields: &BTreeMap<String, String>,
    name: &str,
) -> Result<String, ErrorData> {
    fields
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| invalid(format!("activation URI missing {name}")))
}

fn url_decode(value: &str) -> Result<String, ErrorData> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(invalid("activation URI has truncated percent escape"));
                }
                let hi = hex_value(bytes[i + 1])
                    .ok_or_else(|| invalid("activation URI has bad percent escape"))?;
                let lo = hex_value(bytes[i + 2])
                    .ok_or_else(|| invalid("activation URI has bad percent escape"))?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|error| invalid(format!("activation URI query value is not UTF-8: {error}")))
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
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
    format!("sha256:{}", hex_encode(&digest))
}

pub(crate) fn activation_token_sha256(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

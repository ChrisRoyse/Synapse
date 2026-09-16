//! Integration tests for the AFK escalation engine (#948).
//!
//! These drive the engine over a real `RocksDB` tempdir with synthetic state
//! transitions whose inputs and expected outputs are known, and exercise the
//! Tier 1 off-machine path against a real local HTTP listener. The clock is
//! injected via the `now_unix_ms` parameter so the no-ack ladder and TTL are
//! deterministic.
//!
//! Tier 0 (on-PC Windows toast) is verified separately by manual live-daemon
//! acceptance, so `cargo test` neither pops toasts nor depends on Action Center
//! availability. Tier-1-focused tests pre-mark `tier0_fired` to isolate the
//! off-machine path.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::{Value, json};
use synapse_storage::{Db, cf, decode_json};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::notify_tools::{SYNAPSE_AUMID, SYNAPSE_TOAST_GROUP};
use super::*;

fn db() -> Arc<Db> {
    let dir = tempdir().expect("tempdir");
    let path = dir.keep();
    Arc::new(Db::open(&path, SCHEMA_VERSION).expect("open db"))
}

fn transition(anchor: &str, to: AgentLifecycleState) -> StateTransition {
    StateTransition {
        anchor: anchor.to_owned(),
        spawn_id: Some(anchor.to_owned()),
        session_id: Some(format!("sess-{anchor}")),
        state_from: AgentLifecycleState::Working,
        state_to: to,
        reason_code: "test_reason".to_owned(),
        waiting_for: Some("notify:approval".to_owned()),
        runaway: false,
        evidence: json!({ "alternatives": ["retry", "skip"] }),
    }
}

/// Marks an open escalation as having delivered its Tier 0 toast, so a
/// following `process_pending` exercises only the off-machine path. The toast
/// is also marked already-absent because these tests must not touch Windows
/// Action Center; live Tier 0 delivery/removal is verified manually against the
/// real daemon.
fn mark_tier0_fired(db: &Db, escalation_id: &str, now: u64) {
    let mut item = read_item(db, escalation_id)
        .expect("read item")
        .expect("item present");
    item.tier0_fired = true;
    item.tier0_toast_removed = Some(ToastRemovalOutcome {
        aumid: SYNAPSE_AUMID.to_owned(),
        tag: escalation_toast_tag(escalation_id),
        group: SYNAPSE_TOAST_GROUP.to_owned(),
        status: "not_present".to_owned(),
        removed: false,
        already_absent: true,
        before_count: Some(0),
        after_count: Some(0),
        error_code: None,
        error_message: None,
    });
    item.updated_at_unix_ms = now;
    write_item_and_audit(db, &item, "tier0_toast_fired", json!({ "via": "test" }))
        .expect("mark tier0 fired");
}

fn only_open(db: &Db, anchor: &str) -> EscalationItem {
    let mut open = open_items_for_anchor(db, anchor).expect("scan open");
    assert_eq!(
        open.len(),
        1,
        "expected exactly one open escalation for {anchor}"
    );
    open.remove(0)
}

fn linked_approval(db: &Db, item: &EscalationItem) -> ApprovalItemRecord {
    let key = approval_item_key(&item.approval_id);
    let value = read_exact_row(db, &key)
        .expect("read approval item")
        .expect("linked approval item row");
    decode_json::<ApprovalItemRecord>(&value).expect("decode approval item")
}

fn linked_approval_absent(db: &Db, item: &EscalationItem) {
    let key = approval_item_key(&item.approval_id);
    assert!(
        read_exact_row(db, &key)
            .expect("read approval item")
            .is_none(),
        "suppressed diagnostic escalation must not create a pending approval row"
    );
}

fn approval_audit_events(db: &Db, approval_id: &str) -> Vec<ApprovalAuditRecord> {
    let prefix = format!("{APPROVAL_AUDIT_PREFIX}{approval_id}/");
    db.scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .expect("scan approval audit")
        .into_iter()
        .map(|(_key, value)| decode_json::<ApprovalAuditRecord>(&value).expect("decode audit"))
        .collect()
}

fn assert_linked_approval_ignored(
    db: &Db,
    item: &EscalationItem,
    expected_audit_event: &str,
) -> ApprovalItemRecord {
    let approval = linked_approval(db, item);
    assert_eq!(approval.status, ApprovalStatus::Ignored);
    assert_eq!(
        approval.decided_by_session.as_deref(),
        Some("agent_attention_escalation")
    );
    assert!(
        approval
            .decision_note
            .as_deref()
            .is_some_and(|note| note.contains(&item.escalation_id)),
        "decision note must name linked escalation"
    );
    assert!(
        approval_audit_events(db, &approval.approval_id)
            .iter()
            .any(|audit| audit.event == expected_audit_event
                && audit.before_status == Some(ApprovalStatus::Pending)
                && audit.after_status == ApprovalStatus::Ignored),
        "linked approval must have {expected_audit_event} audit row"
    );
    approval
}

// ---------------------------------------------------------------------------
// Minimal real HTTP listener for Tier 1 webhook regression coverage
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ReceivedRequest {
    headers: String,
    body: Vec<u8>,
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.lines() {
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Binds an ephemeral loopback port and returns its URL plus a receiver that
/// yields each received HTTP request (headers + exact body bytes). Each request
/// is answered `200 OK`.
async fn spawn_webhook_listener() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<ReceivedRequest>,
) {
    spawn_webhook_listener_with_statuses(vec![200]).await
}

async fn spawn_webhook_listener_with_statuses(
    statuses: Vec<u16>,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<ReceivedRequest>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("http://{addr}/hook");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let statuses = Arc::new(statuses);
    let request_index = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _peer)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            let statuses = Arc::clone(&statuses);
            let request_index = Arc::clone(&request_index);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let read = socket.read(&mut tmp).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..read]);
                    if let Some(pos) = find_double_crlf(&buf) {
                        let headers = String::from_utf8_lossy(&buf[..pos]).into_owned();
                        let content_length = parse_content_length(&headers);
                        let body_start = pos + 4;
                        while buf.len() - body_start < content_length {
                            let read = socket.read(&mut tmp).await.unwrap_or(0);
                            if read == 0 {
                                break;
                            }
                            buf.extend_from_slice(&tmp[..read]);
                        }
                        let body =
                            buf[body_start..(body_start + content_length).min(buf.len())].to_vec();
                        let _ = tx.send(ReceivedRequest { headers, body });
                        let index = request_index.fetch_add(1, Ordering::SeqCst);
                        let status = statuses
                            .get(index)
                            .or_else(|| statuses.last())
                            .copied()
                            .unwrap_or(200);
                        let reason = if (200..300).contains(&status) {
                            "OK"
                        } else {
                            "FAIL"
                        };
                        let body = if (200..300).contains(&status) {
                            "ok"
                        } else {
                            "err"
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        return;
                    }
                }
            });
        }
    });
    (url, rx)
}

async fn recv_request(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ReceivedRequest>,
) -> ReceivedRequest {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("webhook request within 5s")
        .expect("listener channel open")
}

fn retention_item(id: &str, status: EscalationStatus, updated_at: u64) -> EscalationItem {
    EscalationItem {
        schema_version: SCHEMA_VERSION,
        escalation_id: id.to_owned(),
        approval_id: format!("apr-{id}"),
        anchor: "retention-agent".to_owned(),
        spawn_id: None,
        session_id: None,
        severity: Severity::Medium,
        attention_state: "needs_input".to_owned(),
        reason_code: None,
        context: EscalationContext {
            action: "retention test".to_owned(),
            reason: "synthetic queue cleanup coverage".to_owned(),
            reversible: true,
            alternatives: Vec::new(),
            waiting_for: None,
            agent_detail_deep_link: "/agents/retention-agent".to_owned(),
            approval_deadline_unix_ms: updated_at.saturating_add(DEFAULT_ACK_WINDOW_MS),
            evidence: Value::Null,
        },
        status,
        created_at_unix_ms: updated_at,
        updated_at_unix_ms: updated_at,
        expires_at_unix_ms: updated_at.saturating_add(DEFAULT_TTL_ORDINARY_MS),
        tier0_fired: false,
        tier0_toast_removed: None,
        tier0_quiet_digest: false,
        tier0_suppressed_reason: None,
        tier1_quiet_suppressed: false,
        tier1_suppressed_reason: None,
        approval_suppressed_reason: Some("retention_test".to_owned()),
        tier1_eligible: false,
        ladder_index: 0,
        next_escalate_at_unix_ms: None,
        channel_attempts: Vec::new(),
        acked_at_unix_ms: None,
        acked_via: None,
        closed_reason: None,
    }
}

#[test]
fn escalation_retention_prunes_only_old_terminal_items() {
    let db = db();
    let now = TERMINAL_ITEM_RETENTION_MS + 10_000;
    let old_terminal = retention_item("esc1-old-terminal", EscalationStatus::Resolved, 1);
    let open_old = retention_item("esc1-open-old", EscalationStatus::Pending, 1);
    let recent_terminal = retention_item(
        "esc1-recent-terminal",
        EscalationStatus::Expired,
        now - TERMINAL_ITEM_RETENTION_MS + 1,
    );
    write_item_and_audit(&db, &old_terminal, "seed", json!({})).expect("old terminal write");
    write_item_and_audit(&db, &open_old, "seed", json!({})).expect("open write");
    write_item_and_audit(&db, &recent_terminal, "seed", json!({})).expect("recent terminal write");

    let deleted = prune_terminal_items(&db, now).expect("prune terminal items");

    assert_eq!(deleted, 1);
    assert!(read_item(&db, "esc1-old-terminal").unwrap().is_none());
    assert!(read_item(&db, "esc1-open-old").unwrap().is_some());
    assert!(read_item(&db, "esc1-recent-terminal").unwrap().is_some());
    let old_audit_prefix = format!("{AUDIT_PREFIX}{}", old_terminal.escalation_id);
    assert!(
        !db.scan_cf_prefix(cf::CF_KV, old_audit_prefix.as_bytes())
            .unwrap()
            .is_empty(),
        "terminal item pruning must leave append-only audit evidence"
    );
}

#[test]
fn escalation_retention_caps_recent_terminal_items() {
    let db = db();
    let now = TERMINAL_ITEM_RETENTION_MS + 100_000;
    let rows = (0..(TERMINAL_ITEM_RETAIN_ROWS + 2))
        .map(|idx| {
            let item = retention_item(
                &format!("esc1-cap-{idx:05}"),
                EscalationStatus::Resolved,
                now - 1_000 + idx as u64,
            );
            (
                item_key(&item.escalation_id),
                encode_item(&item).expect("encode retention item"),
            )
        })
        .collect::<Vec<_>>();
    db.put_batch_pressure_bypass(cf::CF_KV, rows)
        .expect("seed terminal rows");

    let deleted = prune_terminal_items(&db, now).expect("cap terminal items");

    assert_eq!(deleted, 2);
    assert!(read_item(&db, "esc1-cap-00000").unwrap().is_none());
    assert!(read_item(&db, "esc1-cap-00001").unwrap().is_none());
    assert!(read_item(&db, "esc1-cap-00002").unwrap().is_some());
    assert!(
        scan_items(&db).unwrap().len() <= TERMINAL_ITEM_RETAIN_ROWS,
        "terminal cap must keep the hot item queue below the historical failure threshold"
    );
}

// ---------------------------------------------------------------------------
// Decision + durability (no off-machine egress, no toast)
// ---------------------------------------------------------------------------

#[test]
fn opens_pending_escalation_on_attention_transition() {
    let db = db();
    let now = 1_000_000;
    note_transition(
        &db,
        &transition("agent-a", AgentLifecycleState::NeedsInput),
        now,
    );

    // Source of truth: scan CF_KV escalation items.
    let item = only_open(&db, "agent-a");
    assert_eq!(item.status, EscalationStatus::Pending);
    assert_eq!(item.severity, Severity::Medium);
    assert_eq!(item.attention_state, "needs_input");
    assert_eq!(item.context.waiting_for.as_deref(), Some("notify:approval"));
    assert_eq!(item.context.alternatives, vec!["retry", "skip"]);
    assert_eq!(item.context.agent_detail_deep_link, "/agents/agent-a");
    assert_eq!(item.tier0_toast_removed, None);
    // No egress configured ⇒ Tier 0 only, no ladder scheduled.
    assert!(!item.tier1_eligible, "no webhook ⇒ not tier1 eligible");
    assert_eq!(item.next_escalate_at_unix_ms, None);

    // Approval queue linkage physically exists and carries the escalation id.
    let approval = linked_approval(&db, &item);
    assert_eq!(approval.kind, ApprovalKind::AgentEscalation);
    assert_eq!(approval.status, ApprovalStatus::Pending);
    let expected_dedupe = format!("escalation:{}", item.escalation_id);
    assert_eq!(
        approval.dedupe_key.as_deref(),
        Some(expected_dedupe.as_str())
    );
    let payload: Value =
        serde_json::from_str(approval.payload_json.as_deref().unwrap()).expect("payload json");
    assert_eq!(payload["escalation_id"], json!(item.escalation_id));
    assert_eq!(
        payload["context"]["agent_detail_deep_link"],
        json!("/agents/agent-a")
    );
    assert!(
        approval_audit_events(&db, &item.approval_id)
            .iter()
            .any(|audit| audit.event == "requested"),
        "linked approval must have a requested audit row"
    );

    // Audit row physically present.
    let audit_prefix = format!("{AUDIT_PREFIX}{}", item.escalation_id);
    let audit_rows = db
        .scan_cf_prefix(cf::CF_KV, audit_prefix.as_bytes())
        .expect("scan audit");
    assert!(
        audit_rows.iter().any(|(_, v)| {
            serde_json::from_slice::<Value>(v)
                .ok()
                .and_then(|j| j.get("event").and_then(Value::as_str).map(str::to_owned))
                == Some("opened".to_owned())
        }),
        "an 'opened' audit row must exist"
    );
}

#[test]
fn escalation_toast_tag_matches_delivery_dedupe_key() {
    let escalation_id = "esc1-test-tag";
    let dedupe_key = escalation_toast_dedupe_key(escalation_id);
    assert_eq!(dedupe_key, "escalation:esc1-test-tag");
    assert_eq!(
        escalation_toast_tag(escalation_id),
        toast_tag_for(Some(&dedupe_key))
    );
}

#[test]
fn stuck_is_critical_and_ready_for_review_is_low() {
    let db = db();
    note_transition(
        &db,
        &transition("stuck-agent", AgentLifecycleState::Stuck),
        10,
    );
    assert_eq!(only_open(&db, "stuck-agent").severity, Severity::Critical);

    note_transition(
        &db,
        &transition("done-agent", AgentLifecycleState::ReadyForReview),
        10,
    );
    assert_eq!(only_open(&db, "done-agent").severity, Severity::Low);
}

#[test]
fn ambient_unprobeable_silent_timeout_is_policy_acked_without_operator_page() {
    let db = db();
    let anchor = "agent-spawn-ambient-claude-issue1038";
    let transition = StateTransition {
        anchor: anchor.to_owned(),
        spawn_id: Some(anchor.to_owned()),
        session_id: None,
        state_from: AgentLifecycleState::Working,
        state_to: AgentLifecycleState::Stuck,
        reason_code: "silent_timeout_unprobeable".to_owned(),
        waiting_for: Some("silent_for_ms:130000".to_owned()),
        runaway: false,
        evidence: json!({
            "last_event_kind": "tool_call_started",
            "probed_pid": null,
            "silent_ms": 130000,
            "stuck_after_ms": 120000,
        }),
    };

    note_transition(&db, &transition, 1_000);

    let item = only_open(&db, anchor);
    assert_eq!(item.severity, Severity::Critical);
    assert_eq!(item.status, EscalationStatus::Acked);
    assert_eq!(
        item.acked_via.as_deref(),
        Some("policy:ambient_unprobeable_silent_timeout")
    );
    assert_eq!(item.acked_at_unix_ms, Some(1_000));
    assert_eq!(
        item.tier0_suppressed_reason.as_deref(),
        Some("ambient_unprobeable_silent_timeout")
    );
    assert_eq!(
        item.tier1_suppressed_reason.as_deref(),
        Some("ambient_unprobeable_silent_timeout")
    );
    assert_eq!(
        item.approval_suppressed_reason.as_deref(),
        Some("ambient_unprobeable_silent_timeout")
    );
    assert!(!item.tier0_fired, "suppression must not create a toast");
    assert!(
        !item.tier1_eligible,
        "suppression must not route off-machine"
    );
    assert_eq!(item.next_escalate_at_unix_ms, None);
    linked_approval_absent(&db, &item);
    assert!(
        approval_audit_events(&db, &item.approval_id).is_empty(),
        "no approval row means no approval audit spam"
    );
}

#[test]
fn probeable_stuck_agent_still_pages_operator() {
    let db = db();
    let anchor = "agent-spawn-issue1038-probeable";
    let transition = StateTransition {
        anchor: anchor.to_owned(),
        spawn_id: Some(anchor.to_owned()),
        session_id: Some("session-issue1038-probeable".to_owned()),
        state_from: AgentLifecycleState::Working,
        state_to: AgentLifecycleState::Stuck,
        reason_code: "silent_timeout".to_owned(),
        waiting_for: Some("silent_for_ms:130000".to_owned()),
        runaway: false,
        evidence: json!({
            "last_event_kind": "tool_call_started",
            "probed_pid": 4242,
            "silent_ms": 130000,
            "stuck_after_ms": 120000,
        }),
    };

    note_transition(&db, &transition, 2_000);

    let item = only_open(&db, anchor);
    assert_eq!(item.severity, Severity::Critical);
    assert_eq!(item.status, EscalationStatus::Pending);
    assert_eq!(item.tier0_suppressed_reason, None);
    assert_eq!(item.tier1_suppressed_reason, None);
    assert_eq!(item.approval_suppressed_reason, None);
    let approval = linked_approval(&db, &item);
    assert_eq!(approval.status, ApprovalStatus::Pending);
    assert!(
        !approval.toast.suppress_popup,
        "probeable stuck agents still interrupt"
    );
}

#[test]
fn working_idle_spawning_do_not_escalate() {
    let db = db();
    for state in [
        AgentLifecycleState::Working,
        AgentLifecycleState::Idle,
        AgentLifecycleState::Spawning,
        AgentLifecycleState::Dead,
    ] {
        note_transition(&db, &transition("quiet-agent", state), 10);
    }
    assert!(
        open_items_for_anchor(&db, "quiet-agent")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn dedupe_no_double_open_for_same_state() {
    let db = db();
    note_transition(
        &db,
        &transition("agent-b", AgentLifecycleState::NeedsInput),
        100,
    );
    note_transition(
        &db,
        &transition("agent-b", AgentLifecycleState::NeedsInput),
        200,
    );
    assert_eq!(open_items_for_anchor(&db, "agent-b").unwrap().len(), 1);
}

#[test]
fn resolves_on_state_change_out_of_attention() {
    let db = db();
    note_transition(
        &db,
        &transition("agent-c", AgentLifecycleState::NeedsInput),
        100,
    );
    let opened_item = only_open(&db, "agent-c");
    let opened = opened_item.escalation_id.clone();

    // Agent resumes work → escalation auto-resolves.
    note_transition(
        &db,
        &transition("agent-c", AgentLifecycleState::Working),
        200,
    );
    assert!(open_items_for_anchor(&db, "agent-c").unwrap().is_empty());

    let resolved = read_item(&db, &opened).unwrap().unwrap();
    assert_eq!(resolved.status, EscalationStatus::Resolved);
    assert_eq!(
        resolved.closed_reason.as_deref(),
        Some("state_change:working")
    );
    assert_linked_approval_ignored(&db, &resolved, "linked_escalation_resolved");
}

#[test]
fn changing_attention_state_supersedes_old_escalation() {
    let db = db();
    note_transition(
        &db,
        &transition("agent-d", AgentLifecycleState::NeedsInput),
        100,
    );
    note_transition(&db, &transition("agent-d", AgentLifecycleState::Stuck), 200);
    let open = open_items_for_anchor(&db, "agent-d").unwrap();
    assert_eq!(open.len(), 1, "old escalation superseded, one new open");
    assert_eq!(open[0].attention_state, "stuck");
    assert_eq!(open[0].severity, Severity::Critical);
}

#[tokio::test]
async fn process_pending_resolves_terminal_anchor_before_toast() {
    use crate::server::agent_events::record_agent_events;
    use synapse_core::{AgentEventKind, AgentEventRecord};

    let db = db();
    let anchor = format!("issue1010-dead-anchor-{}", Uuid::now_v7().simple());
    let mut exited = AgentEventRecord::new(
        crate::server::agent_events::unix_time_ns_now(),
        AgentEventKind::Exited,
    );
    exited.spawn_id = Some(anchor.clone());
    exited.reason_code = Some("local_model_registry_row_missing".to_owned());
    record_agent_events(&db, &[exited]).expect("dead agent event");

    let transition = StateTransition {
        anchor: anchor.clone(),
        spawn_id: Some(anchor.clone()),
        session_id: None,
        state_from: AgentLifecycleState::Working,
        state_to: AgentLifecycleState::Stuck,
        reason_code: "silent_timeout_unprobeable".to_owned(),
        waiting_for: Some("silent_for_ms:600000".to_owned()),
        runaway: false,
        evidence: json!({ "edge": "stale_open_escalation_after_dead_anchor" }),
    };
    let policy = EscalationPolicy::default();
    let item =
        open_escalation(&db, &transition, Severity::Critical, &policy, 300).expect("open stale");

    let report = process_pending(&db, 301).await.expect("sweep");
    assert_eq!(report.terminal_resolved, 1);
    assert_eq!(report.linked_approvals_closed, 1);
    assert_eq!(report.tier0_fired, 0, "terminal anchors must not toast");

    let readback = read_item(&db, &item.escalation_id)
        .unwrap()
        .expect("item readback");
    assert_eq!(readback.status, EscalationStatus::Resolved);
    assert_eq!(
        readback.closed_reason.as_deref(),
        Some("terminal_agent_state:dead:local_model_registry_row_missing")
    );
    assert_linked_approval_ignored(&db, &readback, "linked_escalation_resolved");
}

#[tokio::test]
async fn process_pending_closes_approval_for_already_resolved_escalation() {
    let db = db();
    note_transition(
        &db,
        &transition("legacy-resolved-agent", AgentLifecycleState::NeedsInput),
        100,
    );
    let mut item = only_open(&db, "legacy-resolved-agent");
    item.status = EscalationStatus::Resolved;
    item.updated_at_unix_ms = 200;
    item.next_escalate_at_unix_ms = None;
    item.closed_reason = Some("legacy_resolved_without_approval_sync".to_owned());
    write_item_and_audit(
        &db,
        &item,
        "resolved",
        json!({ "reason": "legacy_test_without_approval_sync" }),
    )
    .expect("legacy resolved write");
    assert_eq!(linked_approval(&db, &item).status, ApprovalStatus::Pending);

    let report = process_pending(&db, 300).await.expect("sweep");
    assert_eq!(report.linked_approvals_closed, 1);
    let readback = read_item(&db, &item.escalation_id)
        .unwrap()
        .expect("item readback");
    assert_eq!(readback.status, EscalationStatus::Resolved);
    assert_linked_approval_ignored(&db, &readback, "linked_escalation_already_closed");
}

#[tokio::test]
async fn ttl_expiry_closes_linked_approval() {
    let db = db();
    let policy = EscalationPolicy {
        ttl_sensitive_ms: 10,
        ..EscalationPolicy::default()
    };
    let item = open_escalation(
        &db,
        &transition("ttl-agent", AgentLifecycleState::Stuck),
        Severity::Critical,
        &policy,
        100,
    )
    .expect("open");
    assert_eq!(linked_approval(&db, &item).status, ApprovalStatus::Pending);

    let report = process_pending(&db, 111).await.expect("sweep");
    assert_eq!(report.expired, 1);
    assert_eq!(report.linked_approvals_closed, 1);
    let expired = read_item(&db, &item.escalation_id)
        .unwrap()
        .expect("item readback");
    assert_eq!(expired.status, EscalationStatus::Expired);
    assert_linked_approval_ignored(&db, &expired, "linked_escalation_expired");
}

// ---------------------------------------------------------------------------
// Constraint: no operator egress ⇒ zero outbound calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_egress_makes_zero_outbound_calls() {
    let db = db();
    note_transition(
        &db,
        &transition("agent-e", AgentLifecycleState::Stuck),
        1_000,
    );
    let id = only_open(&db, "agent-e").escalation_id;
    // Pre-mark the toast as delivered so process_pending exercises only the
    // (absent) off-machine path without popping a real Windows toast.
    mark_tier0_fired(&db, &id, 1_000);

    let report = process_pending(&db, 2_000).await.expect("process");
    assert_eq!(report.tier1_fired, 0);
    assert_eq!(report.tier1_failed, 0);
    let item = read_item(&db, &id).unwrap().unwrap();
    assert!(
        item.channel_attempts.is_empty(),
        "no channels attempted with no egress"
    );
    assert!(!item.tier1_eligible);
}

// ---------------------------------------------------------------------------
// Tier 1 off-machine delivery (real HTTP listener + HMAC)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tier1_webhook_delivers_signed_packet() {
    let db = db();
    let (url, mut rx) = spawn_webhook_listener().await;
    let secret = "operator-shared-secret";
    let policy = EscalationPolicy {
        webhooks: vec![WebhookChannel {
            name: "ntfy".to_owned(),
            url: url.clone(),
            secret: Some(secret.to_owned()),
        }],
        min_tier1_severity: Severity::Medium,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    note_transition(
        &db,
        &transition("agent-f", AgentLifecycleState::NeedsInput),
        1_000,
    );
    let id = only_open(&db, "agent-f").escalation_id;
    mark_tier0_fired(&db, &id, 1_000);

    let report = process_pending(&db, 1_000).await.expect("process");
    assert_eq!(report.tier1_fired, 1, "one channel fired");

    let received = recv_request(&mut rx).await;
    // Body carries the structured packet with the known escalation id + severity.
    let payload: Value = serde_json::from_slice(&received.body).expect("json body");
    assert_eq!(payload["escalation_id"], json!(id));
    assert_eq!(payload["severity"], json!("medium"));
    assert_eq!(payload["attention_state"], json!("needs_input"));
    assert_eq!(
        payload["context"]["agent_detail_deep_link"],
        json!("/agents/agent-f")
    );

    // HMAC signature header matches the body under the shared secret.
    let expected = format!(
        "sha256={}",
        hmac_sha256_hex(secret.as_bytes(), &received.body)
    );
    assert!(
        received.headers.lines().any(|line| line
            .to_ascii_lowercase()
            .starts_with("x-synapse-signature:")
            && line.contains(&expected)),
        "request must carry a valid HMAC signature; headers=\n{}",
        received.headers
    );

    // Durable readback: attempt recorded ok, ladder advanced.
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 1);
    assert_eq!(item.channel_attempts.len(), 1);
    assert!(item.channel_attempts[0].ok);
    assert_eq!(item.channel_attempts[0].ladder_index, 0);
    assert_eq!(item.channel_attempts[0].attempt_number, 1);
    assert_eq!(item.channel_attempts[0].http_status, Some(200));
    assert!(item.channel_attempts[0].signed);
}

#[tokio::test]
async fn tier1_webhook_failure_retries_same_rung_before_advancing() {
    let db = db();
    let (url, mut rx) = spawn_webhook_listener_with_statuses(vec![500, 200]).await;
    let window = 1_000;
    let policy = EscalationPolicy {
        webhooks: vec![WebhookChannel {
            name: "ntfy".to_owned(),
            url: url.clone(),
            secret: None,
        }],
        min_tier1_severity: Severity::Medium,
        ack_window_ms: window,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    let t0 = 1_000_000;
    note_transition(
        &db,
        &transition("agent-r", AgentLifecycleState::NeedsInput),
        t0,
    );
    let id = only_open(&db, "agent-r").escalation_id;
    mark_tier0_fired(&db, &id, t0);

    let report = process_pending(&db, t0).await.expect("sweep failure");
    assert_eq!(report.tier1_failed, 1);
    let first = recv_request(&mut rx).await;
    let first_payload: Value = serde_json::from_slice(&first.body).expect("json body");
    assert_eq!(first_payload["ladder_index"], json!(0));
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 0, "failed rung must not advance");
    assert_eq!(item.next_escalate_at_unix_ms, Some(t0 + window));
    assert_eq!(item.channel_attempts.len(), 1);
    assert!(!item.channel_attempts[0].ok);
    assert_eq!(item.channel_attempts[0].http_status, Some(500));
    assert_eq!(item.channel_attempts[0].ladder_index, 0);
    assert_eq!(item.channel_attempts[0].attempt_number, 1);

    let report = process_pending(&db, t0 + window)
        .await
        .expect("sweep retry");
    assert_eq!(report.tier1_fired, 1);
    let second = recv_request(&mut rx).await;
    let second_payload: Value = serde_json::from_slice(&second.body).expect("json body");
    assert_eq!(
        second_payload["ladder_index"],
        json!(0),
        "retry must post the same rung"
    );
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 1, "successful retry advances");
    assert_eq!(item.next_escalate_at_unix_ms, None);
    assert_eq!(item.channel_attempts.len(), 2);
    assert!(item.channel_attempts[1].ok);
    assert_eq!(item.channel_attempts[1].http_status, Some(200));
    assert_eq!(item.channel_attempts[1].ladder_index, 0);
    assert_eq!(item.channel_attempts[1].attempt_number, 2);
}

#[tokio::test]
async fn tier1_webhook_failure_exhausts_bounded_retries() {
    let db = db();
    let (url, mut rx) = spawn_webhook_listener_with_statuses(vec![500]).await;
    let window = 1_000;
    let policy = EscalationPolicy {
        webhooks: vec![WebhookChannel {
            name: "ntfy".to_owned(),
            url: url.clone(),
            secret: None,
        }],
        min_tier1_severity: Severity::Medium,
        ack_window_ms: window,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    let t0 = 2_000_000;
    note_transition(
        &db,
        &transition("agent-s", AgentLifecycleState::NeedsInput),
        t0,
    );
    let id = only_open(&db, "agent-s").escalation_id;
    mark_tier0_fired(&db, &id, t0);

    for attempt_number in 1..=WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL {
        let now = t0 + u64::from(attempt_number - 1) * window;
        let report = process_pending(&db, now)
            .await
            .unwrap_or_else(|error| panic!("sweep attempt {attempt_number}: {error}"));
        assert_eq!(report.tier1_failed, 1);
        let _request = recv_request(&mut rx).await;
        let item = read_item(&db, &id).unwrap().unwrap();
        assert_eq!(item.channel_attempts.len(), attempt_number as usize);
        assert_eq!(
            item.channel_attempts
                .last()
                .expect("attempt")
                .attempt_number,
            attempt_number
        );
        if attempt_number < WEBHOOK_RETRY_MAX_ATTEMPTS_PER_CHANNEL {
            assert_eq!(item.ladder_index, 0);
            assert_eq!(item.next_escalate_at_unix_ms, Some(now + window));
        } else {
            assert_eq!(item.ladder_index, 1);
            assert_eq!(item.next_escalate_at_unix_ms, None);
        }
    }
}

#[tokio::test]
async fn no_ack_ladder_fires_second_channel_only_after_window() {
    let db = db();
    let (url, mut rx) = spawn_webhook_listener().await;
    let window = 60_000;
    let policy = EscalationPolicy {
        webhooks: vec![
            WebhookChannel {
                name: "first".to_owned(),
                url: url.clone(),
                secret: None,
            },
            WebhookChannel {
                name: "second".to_owned(),
                url: url.clone(),
                secret: None,
            },
        ],
        min_tier1_severity: Severity::Medium,
        ack_window_ms: window,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    let t0 = 1_000_000;
    note_transition(
        &db,
        &transition("agent-g", AgentLifecycleState::NeedsInput),
        t0,
    );
    let id = only_open(&db, "agent-g").escalation_id;
    mark_tier0_fired(&db, &id, t0);

    // First sweep at t0: channel[0] fires; channel[1] scheduled for t0+window.
    process_pending(&db, t0).await.expect("sweep 1");
    let _first = recv_request(&mut rx).await;
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 1);
    assert_eq!(item.next_escalate_at_unix_ms, Some(t0 + window));

    // Sweep before the window elapses: channel[1] must NOT fire.
    process_pending(&db, t0 + window - 1)
        .await
        .expect("sweep 2");
    assert_eq!(read_item(&db, &id).unwrap().unwrap().ladder_index, 1);

    // Sweep after the window: channel[1] fires; ladder exhausted.
    process_pending(&db, t0 + window).await.expect("sweep 3");
    let _second = recv_request(&mut rx).await;
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 2);
    assert_eq!(item.next_escalate_at_unix_ms, None, "no channels remain");
}

#[tokio::test]
async fn ack_stops_the_ladder() {
    let db = db();
    let (url, mut rx) = spawn_webhook_listener().await;
    let window = 60_000;
    let policy = EscalationPolicy {
        webhooks: vec![
            WebhookChannel {
                name: "first".to_owned(),
                url: url.clone(),
                secret: None,
            },
            WebhookChannel {
                name: "second".to_owned(),
                url: url.clone(),
                secret: None,
            },
        ],
        min_tier1_severity: Severity::Medium,
        ack_window_ms: window,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    let t0 = 1_000_000;
    note_transition(
        &db,
        &transition("agent-h", AgentLifecycleState::NeedsInput),
        t0,
    );
    let id = only_open(&db, "agent-h").escalation_id;
    mark_tier0_fired(&db, &id, t0);

    process_pending(&db, t0).await.expect("sweep 1");
    let _first = recv_request(&mut rx).await;

    // Acknowledge before the second channel's window.
    let acked = ack_escalation(&db, &id, "test_ack", Some("on it"), t0 + 10).expect("ack");
    assert_eq!(acked.status, EscalationStatus::Acked);
    assert_eq!(acked.next_escalate_at_unix_ms, None);

    // Even past the window, no further channel fires.
    process_pending(&db, t0 + window + 1)
        .await
        .expect("sweep 2");
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 1, "ack froze the ladder at one channel");

    // Idempotent ack reports existing state without re-firing.
    let again = ack_escalation(&db, &id, "test_ack_again", None, t0 + 100).expect("ack again");
    assert_eq!(again.status, EscalationStatus::Acked);
    assert_eq!(again.acked_via.as_deref(), Some("test_ack"));
}

#[test]
fn approval_decision_acknowledges_linked_escalation() {
    let db = db();
    let t0 = 1_000_000;
    note_transition(
        &db,
        &transition("agent-approval", AgentLifecycleState::NeedsInput),
        t0,
    );
    let item = only_open(&db, "agent-approval");
    let approval = linked_approval(&db, &item);

    let acked = ack_from_approval_item_decision(
        &db,
        &approval,
        "accept",
        Some("ack from approvals inbox"),
        "test-session",
        t0 + 10,
    )
    .expect("approval bridge")
    .expect("agent escalation approval");

    assert_eq!(acked.status, EscalationStatus::Acked);
    assert_eq!(acked.acked_via.as_deref(), Some("approval_decide:accept"));
    assert_eq!(acked.next_escalate_at_unix_ms, None);
    let readback = read_item(&db, &item.escalation_id).unwrap().unwrap();
    assert_eq!(readback.status, EscalationStatus::Acked);
    assert_eq!(
        readback.acked_via.as_deref(),
        Some("approval_decide:accept")
    );

    let audit_prefix = format!("{AUDIT_PREFIX}{}", item.escalation_id);
    let audit_rows = db
        .scan_cf_prefix(cf::CF_KV, audit_prefix.as_bytes())
        .expect("scan escalation audit");
    assert!(
        audit_rows.iter().any(|(_key, value)| {
            serde_json::from_slice::<Value>(value)
                .ok()
                .and_then(|json| json.get("event").and_then(Value::as_str).map(str::to_owned))
                == Some("acked".to_owned())
        }),
        "approval decision must write an escalation ack audit row"
    );
}

// ---------------------------------------------------------------------------
// Quiet hours (coverage-safe) + TTL expiry
// ---------------------------------------------------------------------------

#[test]
fn quiet_hours_suppresses_medium_but_never_critical() {
    let db = db();
    // A two-minute window centered on the current local minute avoids coupling
    // the test to wall-clock time while still using production quiet-hour logic.
    let minute = current_local_minute_of_day();
    let quiet_start = if minute == 0 { 1439 } else { minute - 1 };
    let quiet_end = (minute + 1) % 1440;
    let policy = EscalationPolicy {
        webhooks: vec![WebhookChannel {
            name: "phone".to_owned(),
            url: "https://example.invalid/hook".to_owned(),
            secret: None,
        }],
        min_tier1_severity: Severity::Medium,
        quiet_hours: Some(QuietHours {
            start_minute: quiet_start,
            end_minute: quiet_end,
        }),
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    note_transition(
        &db,
        &transition("medium-agent", AgentLifecycleState::NeedsInput),
        1_000,
    );
    let medium = only_open(&db, "medium-agent");
    assert!(
        medium.tier1_quiet_suppressed,
        "medium suppressed during quiet hours"
    );
    assert!(
        medium.tier0_quiet_digest,
        "medium toast becomes digest-only during quiet hours"
    );
    assert!(!medium.tier1_eligible, "suppressed ⇒ no off-machine push");
    assert_eq!(medium.next_escalate_at_unix_ms, None);

    note_transition(
        &db,
        &transition("critical-agent", AgentLifecycleState::Stuck),
        1_000,
    );
    let critical = only_open(&db, "critical-agent");
    assert!(
        !critical.tier1_quiet_suppressed,
        "critical never suppressed"
    );
    assert!(
        !critical.tier0_quiet_digest,
        "critical toast still interrupts during quiet hours"
    );
    assert!(critical.tier1_eligible, "critical still routes off-machine");
    assert_eq!(critical.next_escalate_at_unix_ms, Some(1_000));
}

#[test]
fn quiet_hours_wrap_midnight_contains() {
    // 22:00 (1320) → 06:00 (360) overnight window.
    let quiet = QuietHours {
        start_minute: 1320,
        end_minute: 360,
    };
    assert!(quiet.contains(1380)); // 23:00 inside
    assert!(quiet.contains(60)); // 01:00 inside
    assert!(!quiet.contains(720)); // 12:00 outside
}

// ---------------------------------------------------------------------------
// Production trigger path: record_agent_events → emit_transitions → note_transition
// ---------------------------------------------------------------------------

/// Proves the engine is wired at the real `record_agent_events` choke point —
/// not just reachable via a direct `note_transition` call. Drives a synthetic
/// agent through the live state machine (spawn → working → awaiting_approval)
/// and asserts the choke point opened a durable escalation row in CF_KV.
#[test]
fn record_agent_events_choke_point_opens_escalation() {
    use crate::server::agent_events::record_agent_events;
    use synapse_core::{AgentEventKind, AgentEventRecord, GenAiAttributes};

    fn ev(kind: AgentEventKind, spawn: &str, session: Option<&str>) -> AgentEventRecord {
        let mut record =
            AgentEventRecord::new(crate::server::agent_events::unix_time_ns_now(), kind);
        record.spawn_id = Some(spawn.to_owned());
        record.session_id = session.map(ToOwned::to_owned);
        record
    }

    let db = db();
    // A process-unique anchor so the shared in-process state tracker can't be
    // contaminated by (or contaminate) other tests in this binary.
    let spawn = "escalation-wiring-probe-77f3a1";

    // First sight initializes silently (spawning is not attention-worthy).
    record_agent_events(&db, &[ev(AgentEventKind::SpawnRequested, spawn, None)])
        .expect("spawn req");
    assert!(open_items_for_anchor(&db, spawn).unwrap().is_empty());

    // SpawnReady → working (a real transition through emit_transitions; working
    // is not attention-worthy, so still no escalation).
    let mut ready = ev(AgentEventKind::SpawnReady, spawn, Some("sess-wiring"));
    ready.payload = json!({ "launcher_process_id": 4111, "agent_process_id": 4222 });
    record_agent_events(&db, &[ready]).expect("spawn ready");
    assert!(open_items_for_anchor(&db, spawn).unwrap().is_empty());

    // Permission request → working→awaiting_approval. The choke point must now
    // fire the escalation hook and open a Medium escalation.
    let mut approval = ev(AgentEventKind::StateChanged, spawn, None);
    approval.reason_code = Some("permission_request".to_owned());
    approval.state_to = Some("awaiting_approval".to_owned());
    approval.attributes = GenAiAttributes {
        tool_name: Some("Bash".to_owned()),
        ..GenAiAttributes::default()
    };
    record_agent_events(&db, &[approval]).expect("approval");

    // Source of truth: a CF_KV escalation row opened by the production path.
    let open = open_items_for_anchor(&db, spawn).unwrap();
    assert_eq!(
        open.len(),
        1,
        "choke point must open exactly one escalation"
    );
    assert_eq!(open[0].severity, Severity::Medium);
    assert_eq!(open[0].attention_state, "awaiting_approval");
    assert_eq!(open[0].status, EscalationStatus::Pending);

    // And resuming work auto-resolves it through the same path.
    record_agent_events(&db, &[ev(AgentEventKind::TurnStarted, spawn, None)]).expect("turn start");
    assert!(
        open_items_for_anchor(&db, spawn).unwrap().is_empty(),
        "resuming work must auto-resolve the escalation via the choke point"
    );
}

#[tokio::test]
async fn ttl_expires_unacked_escalation() {
    let db = db();
    let policy = EscalationPolicy {
        ttl_ordinary_ms: 500,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    let t0 = 1_000_000;
    note_transition(
        &db,
        &transition("agent-i", AgentLifecycleState::NeedsInput),
        t0,
    );
    let id = only_open(&db, "agent-i").escalation_id;
    assert_eq!(
        read_item(&db, &id).unwrap().unwrap().expires_at_unix_ms,
        t0 + 500
    );

    // Before TTL: still pending.
    mark_tier0_fired(&db, &id, t0);
    process_pending(&db, t0 + 100)
        .await
        .expect("sweep before ttl");
    assert_eq!(
        read_item(&db, &id).unwrap().unwrap().status,
        EscalationStatus::Pending
    );

    // After TTL: expired.
    let report = process_pending(&db, t0 + 500)
        .await
        .expect("sweep after ttl");
    assert_eq!(report.expired, 1);
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.status, EscalationStatus::Expired);
    assert_eq!(item.closed_reason.as_deref(), Some("ttl_expired"));
}

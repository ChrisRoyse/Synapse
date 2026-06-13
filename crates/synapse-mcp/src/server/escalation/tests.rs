//! Integration tests for the AFK escalation engine (#948).
//!
//! These drive the engine over a **real** `RocksDB` (tempdir) with synthetic
//! state transitions whose inputs and expected outputs are known, and exercise
//! the Tier 1 off-machine path against a **real** local HTTP listener — no
//! mocks. The clock is injected via the `now_unix_ms` parameter so the no-ack
//! ladder and TTL are deterministic (no sleeps, no flake).
//!
//! Tier 0 (on-PC Windows toast) is verified separately by a manual live-daemon
//! FSV (plant a real transition, observe the real toast + `verified_in_history`)
//! rather than here, so `cargo test` neither pops toasts nor depends on Action
//! Center availability. Tier-1-focused tests pre-mark `tier0_fired` to isolate
//! the off-machine path.

use std::sync::Arc;

use serde_json::{Value, json};
use synapse_storage::{Db, cf};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
/// following `process_pending` exercises only the off-machine path.
fn mark_tier0_fired(db: &Db, escalation_id: &str, now: u64) {
    let mut item = read_item(db, escalation_id)
        .expect("read item")
        .expect("item present");
    item.tier0_fired = true;
    item.updated_at_unix_ms = now;
    write_item_and_audit(db, &item, "tier0_toast_fired", json!({ "via": "test" }))
        .expect("mark tier0 fired");
}

fn only_open(db: &Db, anchor: &str) -> EscalationItem {
    let mut open = open_items_for_anchor(db, anchor).expect("scan open");
    assert_eq!(open.len(), 1, "expected exactly one open escalation for {anchor}");
    open.remove(0)
}

// ---------------------------------------------------------------------------
// Minimal real HTTP listener for Tier 1 webhook FSV
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
async fn spawn_webhook_listener() -> (String, tokio::sync::mpsc::UnboundedReceiver<ReceivedRequest>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("http://{addr}/hook");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _peer)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
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
                        let body = buf[body_start..(body_start + content_length).min(buf.len())].to_vec();
                        let _ = tx.send(ReceivedRequest { headers, body });
                        let _ = socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await;
                        return;
                    }
                }
            });
        }
    });
    (url, rx)
}

async fn recv_request(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ReceivedRequest>) -> ReceivedRequest {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("webhook request within 5s")
        .expect("listener channel open")
}

// ---------------------------------------------------------------------------
// Decision + durability (no off-machine egress, no toast)
// ---------------------------------------------------------------------------

#[test]
fn opens_pending_escalation_on_attention_transition() {
    let db = db();
    let now = 1_000_000;
    note_transition(&db, &transition("agent-a", AgentLifecycleState::NeedsInput), now);

    // Source of truth: scan CF_KV escalation items.
    let item = only_open(&db, "agent-a");
    assert_eq!(item.status, EscalationStatus::Pending);
    assert_eq!(item.severity, Severity::Medium);
    assert_eq!(item.attention_state, "needs_input");
    assert_eq!(item.context.waiting_for.as_deref(), Some("notify:approval"));
    assert_eq!(item.context.alternatives, vec!["retry", "skip"]);
    assert_eq!(item.context.agent_detail_deep_link, "/agents/agent-a");
    // No egress configured ⇒ Tier 0 only, no ladder scheduled.
    assert!(!item.tier1_eligible, "no webhook ⇒ not tier1 eligible");
    assert_eq!(item.next_escalate_at_unix_ms, None);

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
fn stuck_is_critical_and_ready_for_review_is_low() {
    let db = db();
    note_transition(&db, &transition("stuck-agent", AgentLifecycleState::Stuck), 10);
    assert_eq!(only_open(&db, "stuck-agent").severity, Severity::Critical);

    note_transition(&db, &transition("done-agent", AgentLifecycleState::ReadyForReview), 10);
    assert_eq!(only_open(&db, "done-agent").severity, Severity::Low);
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
    assert!(open_items_for_anchor(&db, "quiet-agent").unwrap().is_empty());
}

#[test]
fn dedupe_no_double_open_for_same_state() {
    let db = db();
    note_transition(&db, &transition("agent-b", AgentLifecycleState::NeedsInput), 100);
    note_transition(&db, &transition("agent-b", AgentLifecycleState::NeedsInput), 200);
    assert_eq!(open_items_for_anchor(&db, "agent-b").unwrap().len(), 1);
}

/// Live Tier 0 FSV: fires a REAL Windows toast through the verified-delivery
/// path. `run_internal_toast` only returns Ok after physically reading the
/// toast back from Action Center history, so `expect` here is genuine
/// verified-in-history proof. Ignored by default so `cargo test` neither pops
/// toasts nor depends on a desktop session; run manually on a logged-in desktop:
///   cargo test -p synapse-mcp --bin synapse-mcp tier0_fires_real_windows_toast -- --ignored --nocapture
#[tokio::test]
#[ignore = "fires a real Windows toast; run manually on a desktop session with --ignored"]
async fn tier0_fires_real_windows_toast() {
    let db = db();
    note_transition(&db, &transition("toast-fsv-agent", AgentLifecycleState::Stuck), 1_000);
    let item = only_open(&db, "toast-fsv-agent");
    assert_eq!(item.severity, Severity::Critical);
    fire_tier0(&item)
        .await
        .expect("real toast delivered AND verified in Action Center history");
}

#[test]
fn resolves_on_state_change_out_of_attention() {
    let db = db();
    note_transition(&db, &transition("agent-c", AgentLifecycleState::NeedsInput), 100);
    let opened = only_open(&db, "agent-c").escalation_id;

    // Agent resumes work → escalation auto-resolves.
    note_transition(&db, &transition("agent-c", AgentLifecycleState::Working), 200);
    assert!(open_items_for_anchor(&db, "agent-c").unwrap().is_empty());

    let resolved = read_item(&db, &opened).unwrap().unwrap();
    assert_eq!(resolved.status, EscalationStatus::Resolved);
    assert_eq!(resolved.closed_reason.as_deref(), Some("state_change:working"));
}

#[test]
fn changing_attention_state_supersedes_old_escalation() {
    let db = db();
    note_transition(&db, &transition("agent-d", AgentLifecycleState::NeedsInput), 100);
    note_transition(&db, &transition("agent-d", AgentLifecycleState::Stuck), 200);
    let open = open_items_for_anchor(&db, "agent-d").unwrap();
    assert_eq!(open.len(), 1, "old escalation superseded, one new open");
    assert_eq!(open[0].attention_state, "stuck");
    assert_eq!(open[0].severity, Severity::Critical);
}

// ---------------------------------------------------------------------------
// Constraint: no operator egress ⇒ zero outbound calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_egress_makes_zero_outbound_calls() {
    let db = db();
    note_transition(&db, &transition("agent-e", AgentLifecycleState::Stuck), 1_000);
    let id = only_open(&db, "agent-e").escalation_id;
    // Pre-mark the toast as delivered so process_pending exercises only the
    // (absent) off-machine path without popping a real Windows toast.
    mark_tier0_fired(&db, &id, 1_000);

    let report = process_pending(&db, 2_000).await.expect("process");
    assert_eq!(report.tier1_fired, 0);
    assert_eq!(report.tier1_failed, 0);
    let item = read_item(&db, &id).unwrap().unwrap();
    assert!(item.channel_attempts.is_empty(), "no channels attempted with no egress");
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

    note_transition(&db, &transition("agent-f", AgentLifecycleState::NeedsInput), 1_000);
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
    assert_eq!(payload["context"]["agent_detail_deep_link"], json!("/agents/agent-f"));

    // HMAC signature header matches the body under the shared secret.
    let expected = format!("sha256={}", hmac_sha256_hex(secret.as_bytes(), &received.body));
    assert!(
        received
            .headers
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("x-synapse-signature:")
                && line.contains(&expected)),
        "request must carry a valid HMAC signature; headers=\n{}",
        received.headers
    );

    // Durable readback: attempt recorded ok, ladder advanced.
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 1);
    assert_eq!(item.channel_attempts.len(), 1);
    assert!(item.channel_attempts[0].ok);
    assert_eq!(item.channel_attempts[0].http_status, Some(200));
    assert!(item.channel_attempts[0].signed);
}

#[tokio::test]
async fn no_ack_ladder_fires_second_channel_only_after_window() {
    let db = db();
    let (url, mut rx) = spawn_webhook_listener().await;
    let window = 60_000;
    let policy = EscalationPolicy {
        webhooks: vec![
            WebhookChannel { name: "first".to_owned(), url: url.clone(), secret: None },
            WebhookChannel { name: "second".to_owned(), url: url.clone(), secret: None },
        ],
        min_tier1_severity: Severity::Medium,
        ack_window_ms: window,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    let t0 = 1_000_000;
    note_transition(&db, &transition("agent-g", AgentLifecycleState::NeedsInput), t0);
    let id = only_open(&db, "agent-g").escalation_id;
    mark_tier0_fired(&db, &id, t0);

    // First sweep at t0: channel[0] fires; channel[1] scheduled for t0+window.
    process_pending(&db, t0).await.expect("sweep 1");
    let _first = recv_request(&mut rx).await;
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 1);
    assert_eq!(item.next_escalate_at_unix_ms, Some(t0 + window));

    // Sweep before the window elapses: channel[1] must NOT fire.
    process_pending(&db, t0 + window - 1).await.expect("sweep 2");
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
            WebhookChannel { name: "first".to_owned(), url: url.clone(), secret: None },
            WebhookChannel { name: "second".to_owned(), url: url.clone(), secret: None },
        ],
        min_tier1_severity: Severity::Medium,
        ack_window_ms: window,
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    let t0 = 1_000_000;
    note_transition(&db, &transition("agent-h", AgentLifecycleState::NeedsInput), t0);
    let id = only_open(&db, "agent-h").escalation_id;
    mark_tier0_fired(&db, &id, t0);

    process_pending(&db, t0).await.expect("sweep 1");
    let _first = recv_request(&mut rx).await;

    // Acknowledge before the second channel's window.
    let acked = ack_escalation(&db, &id, "test_ack", Some("on it"), t0 + 10).expect("ack");
    assert_eq!(acked.status, EscalationStatus::Acked);
    assert_eq!(acked.next_escalate_at_unix_ms, None);

    // Even past the window, no further channel fires.
    process_pending(&db, t0 + window + 1).await.expect("sweep 2");
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.ladder_index, 1, "ack froze the ladder at one channel");

    // Idempotent ack reports existing state without re-firing.
    let again = ack_escalation(&db, &id, "test_ack_again", None, t0 + 100).expect("ack again");
    assert_eq!(again.status, EscalationStatus::Acked);
    assert_eq!(again.acked_via.as_deref(), Some("test_ack"));
}

// ---------------------------------------------------------------------------
// Quiet hours (coverage-safe) + TTL expiry
// ---------------------------------------------------------------------------

#[test]
fn quiet_hours_suppresses_medium_but_never_critical() {
    let db = db();
    // A window covering the entire day so "now" is always inside it.
    let policy = EscalationPolicy {
        webhooks: vec![WebhookChannel {
            name: "phone".to_owned(),
            url: "https://example.invalid/hook".to_owned(),
            secret: None,
        }],
        min_tier1_severity: Severity::Medium,
        quiet_hours: Some(QuietHours { start_minute: 0, end_minute: 1439 }),
        updated_at_unix_ms: 1,
        ..EscalationPolicy::default()
    };
    store_policy(&db, &policy).expect("store policy");

    note_transition(&db, &transition("medium-agent", AgentLifecycleState::NeedsInput), 1_000);
    let medium = only_open(&db, "medium-agent");
    assert!(medium.tier1_quiet_suppressed, "medium suppressed during quiet hours");
    assert!(!medium.tier1_eligible, "suppressed ⇒ no off-machine push");
    assert_eq!(medium.next_escalate_at_unix_ms, None);

    note_transition(&db, &transition("critical-agent", AgentLifecycleState::Stuck), 1_000);
    let critical = only_open(&db, "critical-agent");
    assert!(!critical.tier1_quiet_suppressed, "critical never suppressed");
    assert!(critical.tier1_eligible, "critical still routes off-machine");
    assert_eq!(critical.next_escalate_at_unix_ms, Some(1_000));
}

#[test]
fn quiet_hours_wrap_midnight_contains() {
    // 22:00 (1320) → 06:00 (360) overnight window.
    let quiet = QuietHours { start_minute: 1320, end_minute: 360 };
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
    record_agent_events(&db, &[ev(AgentEventKind::SpawnRequested, spawn, None)]).expect("spawn req");
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
    assert_eq!(open.len(), 1, "choke point must open exactly one escalation");
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
    note_transition(&db, &transition("agent-i", AgentLifecycleState::NeedsInput), t0);
    let id = only_open(&db, "agent-i").escalation_id;
    assert_eq!(read_item(&db, &id).unwrap().unwrap().expires_at_unix_ms, t0 + 500);

    // Before TTL: still pending.
    mark_tier0_fired(&db, &id, t0);
    process_pending(&db, t0 + 100).await.expect("sweep before ttl");
    assert_eq!(read_item(&db, &id).unwrap().unwrap().status, EscalationStatus::Pending);

    // After TTL: expired.
    let report = process_pending(&db, t0 + 500).await.expect("sweep after ttl");
    assert_eq!(report.expired, 1);
    let item = read_item(&db, &id).unwrap().unwrap();
    assert_eq!(item.status, EscalationStatus::Expired);
    assert_eq!(item.closed_reason.as_deref(), Some("ttl_expired"));
}
